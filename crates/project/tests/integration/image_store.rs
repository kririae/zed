use anyhow::Result;
use async_tar::Archive;
use async_trait::async_trait;
use client::{Client, UserStore};
use clock::FakeSystemClock;
use collections::HashMap;
use fs::{
    CopyOptions, CreateOptions, FakeFs, FileHandle, Fs, JobEventReceiver, Metadata, PathEvent,
    RemoveOptions, RenameOptions, Watcher,
};
use futures::{AsyncRead, FutureExt as _, Stream, future::Shared};
use git::repository::GitRepository;
use gpui::{AppContext as _, Entity};
use gpui::TestAppContext;
use image::{DynamicImage, ImageBuffer, Rgba};
use language::LanguageRegistry;
use node_runtime::NodeRuntime;
use parking_lot::Mutex;
use project::Project;
use project::ProjectPath;
use project::image_store::*;
use remote::RemoteClient;
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use serde_json::json;
use settings::SettingsStore;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use text::LineEnding;
use text::Rope;
use util::rel_path::rel_path;

pub fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();

    cx.update(|cx| {
        release_channel::init_test(
            semver::Version::new(0, 0, 0),
            release_channel::ReleaseChannel::Dev,
            cx,
        );
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
}

fn single_pixel_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ]
}

fn single_pixel_exr() -> Vec<u8> {
    let image = DynamicImage::ImageRgba32F(ImageBuffer::from_pixel(
        1,
        1,
        Rgba([1.0_f32, 1.0_f32, 1.0_f32, 1.0_f32]),
    ));
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::OpenExr)
        .expect("writing 1x1 EXR fixture should succeed");
    bytes.into_inner()
}

#[derive(Clone)]
struct BlockingFs {
    inner: Arc<FakeFs>,
    blocked_loads: Arc<Mutex<HashMap<PathBuf, Shared<futures::channel::oneshot::Receiver<()>>>>>,
    release_senders: Arc<Mutex<HashMap<PathBuf, futures::channel::oneshot::Sender<()>>>>,
}

impl BlockingFs {
    fn new(inner: Arc<FakeFs>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            blocked_loads: Default::default(),
            release_senders: Default::default(),
        })
    }

    async fn insert_tree(&self, path: impl AsRef<Path> + Send, contents: serde_json::Value) {
        self.inner.insert_tree(path, contents).await;
    }

    async fn insert_file(&self, path: impl AsRef<Path> + Send, content: Vec<u8>) {
        self.inner.insert_file(path, content).await;
    }

    fn block_path(&self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        let (sender, receiver) = futures::channel::oneshot::channel();
        self.blocked_loads
            .lock()
            .insert(path.clone(), receiver.shared());
        self.release_senders.lock().insert(path, sender);
    }

    fn release_path(&self, path: impl AsRef<Path>) {
        let path = path.as_ref().to_path_buf();
        self.blocked_loads.lock().remove(&path);
        let sender = self
            .release_senders
            .lock()
            .remove(&path)
            .unwrap_or_else(|| panic!("no blocked load registered for {path:?}"));
        sender
            .send(())
            .unwrap_or_else(|()| panic!("blocked load receiver dropped for {path:?}"));
    }
}

#[async_trait]
impl Fs for BlockingFs {
    async fn create_dir(&self, path: &Path) -> Result<()> {
        self.inner.create_dir(path).await
    }

    async fn create_symlink(&self, path: &Path, target: PathBuf) -> Result<()> {
        self.inner.create_symlink(path, target).await
    }

    async fn create_file(&self, path: &Path, options: CreateOptions) -> Result<()> {
        self.inner.create_file(path, options).await
    }

    async fn create_file_with(
        &self,
        path: &Path,
        content: Pin<&mut (dyn AsyncRead + Send)>,
    ) -> Result<()> {
        self.inner.create_file_with(path, content).await
    }

    async fn extract_tar_file(
        &self,
        path: &Path,
        content: Archive<Pin<&mut (dyn AsyncRead + Send)>>,
    ) -> Result<()> {
        self.inner.extract_tar_file(path, content).await
    }

    async fn copy_file(&self, source: &Path, target: &Path, options: CopyOptions) -> Result<()> {
        self.inner.copy_file(source, target, options).await
    }

    async fn rename(&self, source: &Path, target: &Path, options: RenameOptions) -> Result<()> {
        self.inner.rename(source, target, options).await
    }

    async fn remove_dir(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.inner.remove_dir(path, options).await
    }

    async fn remove_file(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.inner.remove_file(path, options).await
    }

    async fn open_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>> {
        self.inner.open_handle(path).await
    }

    async fn open_sync(&self, path: &Path) -> Result<Box<dyn std::io::Read + Send + Sync>> {
        self.inner.open_sync(path).await
    }

    async fn load_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        let receiver = { self.blocked_loads.lock().get(path).cloned() };

        if let Some(receiver) = receiver {
            receiver
                .await
                .map_err(|_| anyhow::anyhow!("blocked load for {path:?} was canceled"))?;
        }

        self.inner.load_bytes(path).await
    }

    async fn atomic_write(&self, path: PathBuf, text: String) -> Result<()> {
        self.inner.atomic_write(path, text).await
    }

    async fn save(&self, path: &Path, text: &Rope, line_ending: LineEnding) -> Result<()> {
        self.inner.save(path, text, line_ending).await
    }

    async fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        self.inner.write(path, content).await
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        self.inner.canonicalize(path).await
    }

    async fn is_file(&self, path: &Path) -> bool {
        self.inner.is_file(path).await
    }

    async fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path).await
    }

    async fn metadata(&self, path: &Path) -> Result<Option<Metadata>> {
        self.inner.metadata(path).await
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf> {
        self.inner.read_link(path).await
    }

    async fn read_dir(
        &self,
        path: &Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<PathBuf>>>>> {
        self.inner.read_dir(path).await
    }

    async fn watch(
        &self,
        path: &Path,
        latency: std::time::Duration,
    ) -> (
        Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
        Arc<dyn Watcher>,
    ) {
        self.inner.watch(path, latency).await
    }

    fn open_repo(
        &self,
        abs_dot_git: &Path,
        system_git_binary_path: Option<&Path>,
    ) -> Option<Arc<dyn GitRepository>> {
        self.inner.open_repo(abs_dot_git, system_git_binary_path)
    }

    async fn git_init(
        &self,
        abs_work_directory: &Path,
        fallback_branch_name: String,
    ) -> Result<()> {
        self.inner
            .git_init(abs_work_directory, fallback_branch_name)
            .await
    }

    async fn git_clone(&self, repo_url: &str, abs_work_directory: &Path) -> Result<()> {
        self.inner.git_clone(repo_url, abs_work_directory).await
    }

    fn is_fake(&self) -> bool {
        self.inner.is_fake()
    }

    async fn is_case_sensitive(&self) -> bool {
        self.inner.is_case_sensitive().await
    }

    fn subscribe_to_jobs(&self) -> JobEventReceiver {
        self.inner.subscribe_to_jobs()
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> Arc<FakeFs> {
        self.inner.clone()
    }
}

struct RemoteImageRequestHandler;

async fn build_remote_image_test_project(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) -> (
    Entity<Project>,
    AnyProtoClient,
    worktree::WorktreeId,
    ImageId,
    Entity<RemoteImageRequestHandler>,
) {
    const REMOTE_IMAGE_ID: u64 = 17;

    init_test(cx);
    init_test(server_cx);

    let (opts, server_session, connect_guard) = RemoteClient::fake_server(cx, server_cx);
    let request_handler = server_cx.update(|cx| cx.new(|_| RemoteImageRequestHandler));

    server_cx.update(|_cx| {
        server_session.subscribe_to_entity(proto::REMOTE_SERVER_PROJECT_ID, &request_handler);
        server_session.add_request_handler(
            request_handler.downgrade(),
            |_entity: Entity<RemoteImageRequestHandler>, _envelope: TypedEnvelope<proto::Ping>, _cx| async move {
                Ok(proto::Ack {})
            },
        );
        server_session.add_request_handler(
            request_handler.downgrade(),
            |_entity: Entity<RemoteImageRequestHandler>,
             envelope: TypedEnvelope<proto::AddWorktree>,
             _cx| async move {
                Ok(proto::AddWorktreeResponse {
                    worktree_id: 1,
                    canonicalized_path: envelope.payload.path,
                })
            },
        );
        server_session.add_entity_request_handler(
            |_entity: Entity<RemoteImageRequestHandler>,
             _envelope: TypedEnvelope<proto::OpenImageByPath>,
             _cx| async move {
                Ok(proto::OpenImageResponse {
                    image_id: REMOTE_IMAGE_ID,
                })
            },
        );
    });

    drop(connect_guard);
    let remote_client = RemoteClient::connect_mock(opts, cx).await;

    let languages = Arc::new(LanguageRegistry::test(cx.executor()));
    let clock = Arc::new(FakeSystemClock::new());
    let http_client = http_client::FakeHttpClient::with_404_response();
    let client = cx.update(|cx| Client::new(clock, http_client, cx));
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

    let project = cx.update(|cx| {
        Project::remote(
            remote_client,
            client,
            NodeRuntime::unavailable(),
            user_store,
            languages,
            fs,
            false,
            cx,
        )
    });

    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree("/remote-root", true, cx)
        })
        .await
        .expect("remote worktree should be created");
    let worktree_id = cx.update(|cx| worktree.read(cx).id());

    (
        project,
        server_session,
        worktree_id,
        ImageId::from(std::num::NonZeroU64::new(REMOTE_IMAGE_ID).expect("remote image id")),
        request_handler,
    )
}

#[gpui::test]
async fn test_image_not_loaded_twice(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree("/root", json!({})).await;
    fs.insert_file("/root/image_1.png", single_pixel_png())
        .await;

    let project = Project::test(fs, ["/root".as_ref()], cx).await;

    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());

    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.png").into(),
    };

    let (task1, task2) = project.update(cx, |project, cx| {
        (
            project.open_image(project_path.clone(), cx),
            project.open_image(project_path.clone(), cx),
        )
    });

    let image1 = task1.await.unwrap();
    let image2 = task2.await.unwrap();

    assert_eq!(image1, image2);
}

#[gpui::test]
async fn test_open_image_returns_placeholder_before_local_bytes_finish_loading(
    cx: &mut TestAppContext,
) {
    init_test(cx);
    let blocking = BlockingFs::new(FakeFs::new(cx.executor()));

    blocking.insert_tree("/root", json!({})).await;
    blocking
        .insert_file("/root/image_1.png", single_pixel_png())
        .await;
    blocking.block_path("/root/image_1.png");

    let project = Project::test(blocking.clone(), ["/root".as_ref()], cx).await;

    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.png").into(),
    };

    let open_task = project.update(cx, |project, cx| project.open_image(project_path, cx));
    cx.run_until_parked();
    cx.executor().run_until_parked();
    cx.run_until_parked();

    let image_item = open_task
        .now_or_never()
        .expect("open_image should return a placeholder entity immediately")
        .expect("placeholder entity should not be an error");

    let (is_loading, has_image, metadata) = cx.update(|cx| {
        let item = image_item.read(cx);
        (item.is_loading(), item.image.is_some(), item.image_metadata)
    });

    assert!(is_loading);
    assert!(!has_image);
    assert!(metadata.is_none());

    blocking.release_path("/root/image_1.png");
    cx.executor().run_until_parked();
    cx.run_until_parked();
}

#[gpui::test]
async fn test_placeholder_image_item_loads_in_place_after_release(cx: &mut TestAppContext) {
    init_test(cx);
    let blocking = BlockingFs::new(FakeFs::new(cx.executor()));

    blocking.insert_tree("/root", json!({})).await;
    blocking
        .insert_file("/root/image_1.png", single_pixel_png())
        .await;
    blocking.block_path("/root/image_1.png");

    let project = Project::test(blocking.clone(), ["/root".as_ref()], cx).await;
    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.png").into(),
    };

    let open_task = project.update(cx, |project, cx| project.open_image(project_path, cx));
    cx.run_until_parked();
    cx.executor().run_until_parked();
    cx.run_until_parked();

    let image_item = open_task
        .now_or_never()
        .expect("open_image should return a placeholder entity immediately")
        .expect("placeholder entity should not be an error");
    let image_entity_id = cx.update(|_cx| image_item.entity_id());

    blocking.release_path("/root/image_1.png");
    cx.executor().run_until_parked();
    cx.run_until_parked();

    let (loaded_entity_id, load_state, has_image, metadata) = cx.update(|cx| {
        let item = image_item.read(cx);
        (
            image_item.entity_id(),
            item.load_state.clone(),
            item.image.is_some(),
            item.image_metadata,
        )
    });

    assert_eq!(loaded_entity_id, image_entity_id);
    assert_eq!(load_state, ImageLoadState::Loaded);
    assert!(has_image);
    assert!(metadata.is_some());
}

#[gpui::test]
async fn test_remote_open_image_returns_placeholder_before_remote_content_arrives(
    cx: &mut TestAppContext,
    server_cx: &mut TestAppContext,
) {
    let (project, server_session, worktree_id, remote_image_id, _request_handler) =
        build_remote_image_test_project(cx, server_cx).await;
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.png").into(),
    };

    let open_task = project.update(cx, |project, cx| project.open_image(project_path, cx));
    server_cx.run_until_parked();
    cx.run_until_parked();
    cx.executor().run_until_parked();
    server_cx.executor().run_until_parked();
    cx.run_until_parked();

    let image_item = open_task
        .now_or_never()
        .expect("remote open_image should return a placeholder entity immediately")
        .expect("placeholder entity should not be an error");
    let image_entity_id = image_item.entity_id();

    let (is_loading, has_image, metadata) = cx.update(|cx| {
        let item = image_item.read(cx);
        (item.is_loading(), item.image.is_some(), item.image_metadata)
    });

    assert!(is_loading);
    assert!(!has_image);
    assert!(metadata.is_none());

    let png_bytes = single_pixel_png();
    server_session
        .send(proto::CreateImageForPeer {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            peer_id: Some(proto::REMOTE_SERVER_PEER_ID),
            variant: Some(proto::create_image_for_peer::Variant::State(
                proto::ImageState {
                    id: remote_image_id.to_proto(),
                    file: Some(proto::File {
                        worktree_id: worktree_id.to_proto(),
                        entry_id: None,
                        path: "image_1.png".into(),
                        mtime: None,
                        is_deleted: false,
                        is_historic: false,
                    }),
                    content_size: png_bytes.len() as u64,
                    format: "png".into(),
                },
            )),
        })
        .expect("sending remote image state should succeed");
    server_session
        .send(proto::CreateImageForPeer {
            project_id: proto::REMOTE_SERVER_PROJECT_ID,
            peer_id: Some(proto::REMOTE_SERVER_PEER_ID),
            variant: Some(proto::create_image_for_peer::Variant::Chunk(
                proto::ImageChunk {
                    image_id: remote_image_id.to_proto(),
                    data: png_bytes,
                },
            )),
        })
        .expect("sending remote image chunk should succeed");

    cx.condition(&image_item, |image_item, _cx| {
        matches!(image_item.load_state, ImageLoadState::Loaded)
            && image_item.image.is_some()
            && image_item.image_metadata.is_some()
    })
    .await;

    let (loaded_entity_id, load_state, has_image, metadata) = cx.update(|cx| {
        let item = image_item.read(cx);
        (
            image_item.entity_id(),
            item.load_state.clone(),
            item.image.is_some(),
            item.image_metadata,
        )
    });

    assert_eq!(loaded_entity_id, image_entity_id);
    assert_eq!(load_state, ImageLoadState::Loaded);
    assert!(has_image);
    assert!(metadata.is_some());
}

#[gpui::test]
async fn test_reload_failure_preserves_previous_image_and_metadata(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree("/root", json!({})).await;
    fs.insert_file("/root/image_1.png", single_pixel_png()).await;

    let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());
    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.png").into(),
    };

    let image = project
        .update(cx, |project, cx| project.open_image(project_path, cx))
        .await
        .unwrap();

    cx.condition(&image, |image, _cx| image.image_metadata.is_some())
        .await;

    let initial_metadata = cx
        .update(|cx| image.read(cx).image_metadata)
        .expect("loaded image should have metadata");

    fs.pause_events();
    fs.insert_file("/root/image_1.png", b"not an image".to_vec()).await;

    project
        .update(cx, |project, cx| {
            project.reload_images([image.clone()].into_iter().collect(), cx)
        })
        .await
        .expect("reload task should finish even when decoding fails");

    let (load_state, has_image, metadata) = cx.update(|cx| {
        let image = image.read(cx);
        (
            image.load_state.clone(),
            image.image.is_some(),
            image.image_metadata,
        )
    });

    assert!(matches!(load_state, ImageLoadState::Failed(_)));
    assert!(has_image, "reload failure should preserve the last good image");
    assert_eq!(
        metadata,
        Some(initial_metadata),
        "reload failure should preserve the last good metadata"
    );

    fs.unpause_events_and_flush();
}

#[gpui::test]
fn test_compute_metadata_from_bytes() {
    // Single white pixel PNG
    let png_bytes = single_pixel_png();

    let metadata = ImageItem::compute_metadata_from_bytes(&png_bytes).unwrap();

    assert_eq!(metadata.width, 1);
    assert_eq!(metadata.height, 1);
    assert_eq!(metadata.file_size, png_bytes.len() as u64);
    assert_eq!(metadata.format, image::ImageFormat::Png);
    assert!(metadata.colors.is_some());
}

#[gpui::test]
fn test_compute_metadata_from_exr_bytes() {
    let exr_bytes = single_pixel_exr();

    let metadata = ImageItem::compute_metadata_from_bytes(&exr_bytes).unwrap();

    assert_eq!(metadata.width, 1);
    assert_eq!(metadata.height, 1);
    assert_eq!(metadata.file_size, exr_bytes.len() as u64);
    assert_eq!(metadata.format, image::ImageFormat::OpenExr);
}

#[gpui::test]
async fn test_open_exr_image(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    let exr_bytes = single_pixel_exr();

    fs.insert_tree("/root", json!({})).await;
    fs.insert_file("/root/image_1.exr", exr_bytes.clone()).await;

    let project = Project::test(fs, ["/root".as_ref()], cx).await;

    let worktree_id = cx.update(|cx| project.read(cx).worktrees(cx).next().unwrap().read(cx).id());

    let project_path = ProjectPath {
        worktree_id,
        path: rel_path("image_1.exr").into(),
    };

    let image = project
        .update(cx, |project, cx| project.open_image(project_path, cx))
        .await
        .unwrap();

    cx.condition(&image, |image, _cx| image.image_metadata.is_some())
        .await;

    let metadata = cx
        .update(|cx| image.read(cx).image_metadata)
        .expect("opened EXR image should have metadata");

    assert_eq!(metadata.width, 1);
    assert_eq!(metadata.height, 1);
    assert_eq!(metadata.file_size, exr_bytes.len() as u64);
    assert_eq!(metadata.format, image::ImageFormat::OpenExr);
}
