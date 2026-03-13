use fs::FakeFs;
use gpui::TestAppContext;
use image::{DynamicImage, ImageBuffer, Rgba};
use project::Project;
use project::ProjectPath;
use project::image_store::*;
use serde_json::json;
use settings::SettingsStore;
use std::io::Cursor;
use util::rel_path::rel_path;

pub fn init_test(cx: &mut TestAppContext) {
    zlog::init_test();

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
    });
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

#[gpui::test]
async fn test_image_not_loaded_twice(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());

    fs.insert_tree("/root", json!({})).await;
    // Create a png file that consists of a single white pixel
    fs.insert_file(
        "/root/image_1.png",
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ],
    )
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
fn test_compute_metadata_from_bytes() {
    // Single white pixel PNG
    let png_bytes = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

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

    let metadata = cx
        .update(|cx| image.read(cx).image_metadata)
        .expect("opened EXR image should have metadata");

    assert_eq!(metadata.width, 1);
    assert_eq!(metadata.height, 1);
    assert_eq!(metadata.file_size, exr_bytes.len() as u64);
    assert_eq!(metadata.format, image::ImageFormat::OpenExr);
}
