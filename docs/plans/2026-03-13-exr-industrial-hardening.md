# EXR Industrial Hardening Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the remaining correctness and coverage gaps so Zed's EXR support is safe to ship beyond the local happy path.

**Architecture:** Keep the current EXR viewer integration, but harden the platform helper branches that were touched by the new `ImageFormat::Exr` enum case. The safest approach is to normalize clipboard behavior around concrete, readable formats and add one real remote integration test so the distributed image path is exercised instead of assumed.

**Tech Stack:** Rust, `gpui`, `project`, `remote_server`, platform clipboard code, Cargo unit and integration tests

---

### Task 1: Fix X11 clipboard EXR payload encoding

**Files:**
- Modify: `crates/gpui_linux/src/linux/x11/clipboard.rs`
- Test: `crates/gpui_linux/src/linux/x11/clipboard.rs`

**Step 1: Write the failing unit test**

Add a pure helper plus unit tests in `crates/gpui_linux/src/linux/x11/clipboard.rs` so the EXR clipboard behavior can be tested without a live X server.

Target helper shape:

```rust
fn prepare_image_for_clipboard(image: &Image) -> Result<Image> {
    if image.format != ImageFormat::Exr {
        return Ok(image.clone());
    }

    let dynamic_image =
        image::load_from_memory_with_format(&image.bytes, image::ImageFormat::OpenExr)
            .map_err(|_| Error::ConversionFailure)?;
    let rgba = dynamic_image.into_rgba8();

    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|_| Error::ConversionFailure)?;

    Ok(Image::from_bytes(ImageFormat::Png, png_bytes))
}
```

Add tests like:

```rust
#[test]
fn test_prepare_exr_image_for_clipboard_transcodes_to_png() {
    let exr = Image::from_bytes(ImageFormat::Exr, single_pixel_exr_bytes());

    let prepared = prepare_image_for_clipboard(&exr).unwrap();

    assert_eq!(prepared.format, ImageFormat::Png);
    assert_eq!(
        image::guess_format(&prepared.bytes).unwrap(),
        image::ImageFormat::Png
    );
}
```

```rust
#[test]
fn test_prepare_png_image_for_clipboard_is_passthrough() {
    let png = Image::from_bytes(ImageFormat::Png, single_pixel_png_bytes());
    let prepared = prepare_image_for_clipboard(&png).unwrap();
    assert_eq!(prepared.format, ImageFormat::Png);
    assert_eq!(prepared.bytes, png.bytes);
}
```

**Step 2: Run the test to verify it fails**

Run:

```bash
cargo test -p gpui_linux test_prepare_exr_image_for_clipboard_transcodes_to_png --lib -- --nocapture
```

Expected: FAIL because `set_image` still writes raw EXR bytes and no helper exists yet.

**Step 3: Implement the minimal fix**

Update `Clipboard::set_image` in `crates/gpui_linux/src/linux/x11/clipboard.rs` to use the helper before choosing the MIME atom:

```rust
let image = prepare_image_for_clipboard(&image)?;
let format = match image.format {
    ImageFormat::Png => self.inner.atoms.PNG__MIME,
    ImageFormat::Jpeg => self.inner.atoms.JPEG_MIME,
    ImageFormat::Webp => self.inner.atoms.WEBP_MIME,
    ImageFormat::Gif => self.inner.atoms.GIF__MIME,
    ImageFormat::Svg => self.inner.atoms.SVG__MIME,
    ImageFormat::Bmp => self.inner.atoms.BMP__MIME,
    ImageFormat::Tiff => self.inner.atoms.TIFF_MIME,
    ImageFormat::Ico => self.inner.atoms.ICO__MIME,
    ImageFormat::Exr => return Err(Error::ConversionFailure),
};

let data = vec![ClipboardData {
    bytes: image.bytes,
    format,
}];
```

The important part is that advertised MIME and actual bytes now match.

**Step 4: Run the tests to verify they pass**

Run:

```bash
cargo test -p gpui_linux test_prepare_exr_image_for_clipboard_transcodes_to_png --lib -- --nocapture
cargo test -p gpui_linux test_prepare_png_image_for_clipboard_is_passthrough --lib -- --nocapture
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/gpui_linux/src/linux/x11/clipboard.rs
git commit -m "Fix X11 EXR clipboard payload encoding"
```

### Task 2: Make macOS EXR clipboard fallback safe and testable

**Files:**
- Modify: `crates/gpui_macos/src/pasteboard.rs`
- Test: `crates/gpui_macos/src/pasteboard.rs`

**Step 1: Write the failing unit tests around the pure preparation step**

Refactor the EXR-specific preparation logic in `crates/gpui_macos/src/pasteboard.rs` into a pure helper that can be tested before touching `clearContents()`:

```rust
fn prepare_image_for_pasteboard(image: &Image) -> Option<(Vec<u8>, ImageFormat)> {
    if image.format != ImageFormat::Exr {
        return Some((image.bytes.clone(), image.format));
    }

    let dynamic_image =
        image::load_from_memory_with_format(&image.bytes, image::ImageFormat::OpenExr).ok()?;
    let rgba = dynamic_image.into_rgba8();
    let mut png_bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png_bytes)
        .write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;

    Some((png_bytes, ImageFormat::Png))
}
```

Add tests:

```rust
#[test]
fn test_prepare_exr_image_for_pasteboard_transcodes_to_png() {
    let image = Image::from_bytes(ImageFormat::Exr, single_pixel_exr_bytes());
    let (bytes, format) = prepare_image_for_pasteboard(&image).unwrap();
    assert_eq!(format, ImageFormat::Png);
    assert_eq!(image::guess_format(&bytes).unwrap(), image::ImageFormat::Png);
}
```

```rust
#[test]
fn test_prepare_invalid_exr_image_for_pasteboard_returns_none() {
    let image = Image::from_bytes(ImageFormat::Exr, vec![1, 2, 3, 4]);
    assert!(prepare_image_for_pasteboard(&image).is_none());
}
```

**Step 2: Run the test to verify it fails**

Run on macOS:

```bash
cargo test -p gpui_macos test_prepare_exr_image_for_pasteboard_transcodes_to_png --lib -- --nocapture
```

Expected: FAIL because the helper does not exist yet.

**Step 3: Implement the minimal safety fix**

Update `write_image` to prepare first and only clear the pasteboard after preparation succeeds:

```rust
let Some((bytes, format)) = prepare_image_for_pasteboard(image) else {
    return;
};

self.inner.clearContents();
let bytes = NSData::dataWithBytes_length_(nil, bytes.as_ptr() as *const c_void, bytes.len() as u64);
self.inner
    .setData_forType(bytes, Into::<UTType>::into(format).inner_mut());
```

This preserves the current clipboard contents when EXR decoding or PNG encoding fails.

**Step 4: Run the tests to verify they pass**

Run on macOS:

```bash
cargo test -p gpui_macos test_prepare_exr_image_for_pasteboard_transcodes_to_png --lib -- --nocapture
cargo test -p gpui_macos test_prepare_invalid_exr_image_for_pasteboard_returns_none --lib -- --nocapture
```

Expected: PASS.

**Step 5: Commit**

```bash
git add crates/gpui_macos/src/pasteboard.rs
git commit -m "Make macOS EXR clipboard fallback safe"
```

### Task 3: Add a real remote EXR integration test

**Files:**
- Modify: `crates/remote_server/src/remote_editing_tests.rs`
- Reference: `crates/remote_server/src/headless_project.rs`
- Reference: `crates/project/src/image_store.rs`

**Step 1: Write the failing remote test**

In `crates/remote_server/src/remote_editing_tests.rs`, add a helper to build a tiny EXR fixture and a new test:

```rust
fn single_pixel_exr_bytes() -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba32F(image::ImageBuffer::from_pixel(
        1,
        1,
        image::Rgba([1.0_f32, 0.5_f32, 0.25_f32, 1.0_f32]),
    ));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::OpenExr)
        .unwrap();
    bytes.into_inner()
}

#[gpui::test]
async fn test_remote_open_exr_image(cx: &mut TestAppContext, server_cx: &mut TestAppContext) {
    let fs = FakeFs::new(server_cx.executor());
    fs.insert_tree(
        path!("/code"),
        json!({
            "project1": {
                "images": {}
            }
        }),
    )
    .await;
    fs.insert_file("/code/project1/images/test.exr", single_pixel_exr_bytes())
        .await;

    let (project, _headless) = init_test(&fs, cx, server_cx).await;
    let (worktree, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/code/project1"), true, cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();

    let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());
    let image = project
        .update(cx, |project, cx| {
            project.open_image((worktree_id, rel_path("images/test.exr")), cx)
        })
        .await
        .unwrap();

    image.update(cx, |image, _| {
        let metadata = image.image_metadata.expect("remote EXR should have metadata");
        assert_eq!(metadata.format, image::ImageFormat::OpenExr);
        assert_eq!(metadata.width, 1);
        assert_eq!(metadata.height, 1);
    });
}
```

**Step 2: Run the test to verify it fails or exposes a missing path**

Run:

```bash
cargo test -p remote_server --features test-support test_remote_open_exr_image -- --nocapture
```

Expected: FAIL if the remote image assembly path still has EXR-specific gaps. If it passes immediately, keep the test anyway because it is the first real remote regression coverage for this feature.

**Step 3: Implement the minimal code only if the test exposes a gap**

Only touch the code path the test proves is broken. Likely candidates are:

- `crates/project/src/image_store.rs`
- `crates/remote_server/src/headless_project.rs`

If the test already passes, skip implementation edits and keep this task as a pure coverage addition.

**Step 4: Re-run the test to verify it passes**

Run:

```bash
cargo test -p remote_server --features test-support test_remote_open_exr_image -- --nocapture
```

Expected: PASS.

**Step 5: Commit**

If this task added only the test:

```bash
git add crates/remote_server/src/remote_editing_tests.rs
git commit -m "Add remote EXR image integration test"
```

If it also required implementation changes, include those files in the same commit with:

```bash
git add crates/remote_server/src/remote_editing_tests.rs crates/project/src/image_store.rs crates/remote_server/src/headless_project.rs
git commit -m "Harden remote EXR image loading"
```

### Task 4: Run final verification for the hardened EXR path

**Files:**
- No planned code changes

**Step 1: Run Linux clipboard tests**

Run on Linux:

```bash
cargo test -p gpui_linux prepare_exr_image_for_clipboard --lib -- --nocapture
```

Expected: PASS.

**Step 2: Run macOS pasteboard tests**

Run on macOS:

```bash
cargo test -p gpui_macos prepare_exr_image_for_pasteboard --lib -- --nocapture
```

Expected: PASS.

**Step 3: Run the project EXR regressions**

Run:

```bash
cargo test -p project --features test-support --test integration image_store:: -- --nocapture
```

Expected: PASS.

**Step 4: Run the remote EXR integration test**

Run:

```bash
cargo test -p remote_server --features test-support test_remote_open_exr_image -- --nocapture
```

Expected: PASS.

**Step 5: Run the app compile check**

Run:

```bash
cargo check -p zed
```

Expected: PASS.

**Step 6: Commit only if verification forced further fixes**

```bash
git status
```

Expected: clean working tree. If not clean because verification uncovered another EXR edge case, commit that fix with a precise message before closing.
