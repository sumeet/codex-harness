use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use gpui::{
    AnyElement, App, Context, Image, ImageFormat, ImageSource, IntoElement, ObjectFit, Render,
    RenderImage, SharedString, StyledImage, Task, Window, div, prelude::*,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon, LabelSize};

const IMAGE_PREVIEW_MAX_WIDTH: f32 = 384.;
const IMAGE_PREVIEW_MAX_HEIGHT: f32 = 240.;
const IMAGE_PREVIEW_FALLBACK_WIDTH: f32 = 320.;
const IMAGE_PREVIEW_FALLBACK_HEIGHT: f32 = 180.;
const IMAGE_ROW_HEIGHT: f32 = 20.;
const IMAGE_PLACEHOLDER_ROWS: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ImageAvailability {
    Present {
        path: PathBuf,
        image: Arc<Image>,
        dimensions: Option<(u32, u32)>,
    },
    Loading(PathBuf),
    MissingPath,
    Unavailable(PathBuf, String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SurfaceSyncDecision {
    Ignore,
    Remove,
    Upsert,
}

pub(crate) fn surface_sync_decision(
    item_is_image: bool,
    surface_exists: bool,
) -> SurfaceSyncDecision {
    match (item_is_image, surface_exists) {
        (true, _) => SurfaceSyncDecision::Upsert,
        (false, true) => SurfaceSyncDecision::Remove,
        (false, false) => SurfaceSyncDecision::Ignore,
    }
}

pub(crate) fn keys_to_sync(
    existing_surface_keys: impl IntoIterator<Item = String>,
    projected_image_keys: impl IntoIterator<Item = String>,
) -> HashSet<String> {
    existing_surface_keys
        .into_iter()
        .chain(projected_image_keys)
        .collect()
}

pub(crate) struct ImageSurface {
    availability: ImageAvailability,
    path: Option<PathBuf>,
    snapshot_identity: String,
    load_task: Task<()>,
}

impl ImageSurface {
    pub(crate) fn new(raw: &Value, snapshot_identity: String, cx: &mut Context<Self>) -> Self {
        let mut surface = Self {
            availability: ImageAvailability::MissingPath,
            path: None,
            snapshot_identity,
            load_task: Task::ready(()),
        };
        surface.update(raw, cx);
        surface
    }

    pub(crate) fn update(&mut self, raw: &Value, cx: &mut Context<Self>) {
        let path = image_path(raw);
        if self.path == path && !matches!(self.availability, ImageAvailability::Unavailable(..)) {
            return;
        }
        self.path = path.clone();
        let Some(path) = path else {
            self.availability = ImageAvailability::MissingPath;
            self.load_task = Task::ready(());
            cx.notify();
            return;
        };
        self.availability = ImageAvailability::Loading(path.clone());
        let cache_path = dirs::cache_dir().map(|root| {
            snapshot_path(
                &root.join("harness/image-snapshots"),
                &self.snapshot_identity,
                &path,
            )
        });
        let load = cx.background_spawn(async move {
            match load_snapshot(&path, cache_path.as_deref()) {
                Ok(image) => ImageAvailability::Present {
                    dimensions: super::image_dimensions(&image.bytes, image.format),
                    path,
                    image,
                },
                Err(error) => {
                    log::warn!(
                        "could not load transcript image {}: {error:#}",
                        path.display()
                    );
                    ImageAvailability::Unavailable(path, error.to_string())
                }
            }
        });
        self.load_task = cx.spawn(async move |this, cx| {
            let availability = load.await;
            if let Err(error) = this.update(cx, |this, cx| {
                this.availability = availability;
                cx.notify();
            }) {
                log::debug!("image surface closed while loading: {error}");
            }
        });
        cx.notify();
    }

    pub(crate) fn preview_size(&self) -> (f32, f32) {
        preview_size_for_availability(&self.availability)
    }

    pub(crate) fn preview_source(&self) -> Option<ImageSource> {
        match &self.availability {
            ImageAvailability::Present { image, .. } => Some(image.clone().into()),
            _ => None,
        }
    }
}

fn image_path(raw: &Value) -> Option<PathBuf> {
    raw.pointer("/path")
        .or_else(|| raw.pointer("/savedPath"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn snapshot_path(root: &Path, identity: &str, path: &Path) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(identity.as_bytes());
    digest.update([0]);
    digest.update(path.as_os_str().as_encoded_bytes());
    root.join(format!("{:x}", digest.finalize()))
}

fn read_image(path: &Path) -> anyhow::Result<Arc<Image>> {
    const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_IMAGE_BYTES,
        "Image exceeds 64 MiB"
    );
    let format = image::guess_format(&bytes)
        .ok()
        .and_then(|format| ImageFormat::from_mime_type(format.to_mime_type()))
        .or_else(|| {
            std::str::from_utf8(&bytes)
                .ok()
                .filter(|text| text.trim_start().starts_with("<svg") || text.contains("<svg "))
                .map(|_| ImageFormat::Svg)
        })
        .context("Unsupported image format")?;
    Ok(Arc::new(Image::from_bytes(format, bytes)))
}

fn load_snapshot(path: &Path, cache_path: Option<&Path>) -> anyhow::Result<Arc<Image>> {
    if let Some(cache_path) = cache_path {
        match read_image(cache_path) {
            Ok(image) => return Ok(image),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error.context("Cannot read saved image snapshot")),
        }
    }
    let image = read_image(path).context("Image file is unavailable or unreadable")?;
    if let Some(cache_path) = cache_path {
        // A filename is mutable, but a transcript event is not. Publish once,
        // atomically, so reopening a thread cannot replace an earlier view with
        // the latest screenshot (or race another Harness window's snapshot).
        match persist_snapshot(cache_path, &image.bytes) {
            Ok(()) => return read_image(cache_path),
            Err(error) => log::warn!("could not save image snapshot: {error:#}"),
        }
    }
    Ok(image)
}

fn persist_snapshot(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let directory = path
        .parent()
        .context("Image snapshot has no parent directory")?;
    fs::create_dir_all(directory)?;
    let temporary = directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = file
        .write_all(bytes)
        .and_then(|()| match fs::hard_link(&temporary, path) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            result => result,
        });
    if let Err(error) = fs::remove_file(&temporary) {
        log::warn!(
            "could not remove image snapshot temporary {}: {error}",
            temporary.display()
        );
    }
    result?;
    Ok(())
}

pub(crate) fn lightbox_image(
    source: &ImageSource,
    window: &mut Window,
    cx: &mut App,
) -> Option<Arc<RenderImage>> {
    match source {
        ImageSource::Image(image) => image.clone().use_render_image(window, cx),
        ImageSource::Render(image) => Some(image.clone()),
        ImageSource::Resource(resource) => window
            .use_asset::<gpui::ImgResourceLoader>(resource, cx)
            .and_then(Result::ok),
        ImageSource::Custom(load) => load(window, cx).and_then(Result::ok),
    }
}

pub(crate) fn lightbox_size(image: (f32, f32), viewport: (f32, f32)) -> (f32, f32) {
    let scale = ((viewport.0 - 48.).max(1.) / image.0.max(1.))
        .min((viewport.1 - 48.).max(1.) / image.1.max(1.))
        .min(1.);
    (image.0 * scale, image.1 * scale)
}

fn preview_size(dimensions: Option<(u32, u32)>) -> (f32, f32) {
    let Some((width, height)) = dimensions.filter(|(width, height)| *width > 0 && *height > 0)
    else {
        return (IMAGE_PREVIEW_FALLBACK_WIDTH, IMAGE_PREVIEW_FALLBACK_HEIGHT);
    };
    let scale = (IMAGE_PREVIEW_MAX_WIDTH / width as f32)
        .min(IMAGE_PREVIEW_MAX_HEIGHT / height as f32)
        .min(1.);
    (width as f32 * scale, height as f32 * scale)
}

fn preview_size_for_availability(availability: &ImageAvailability) -> (f32, f32) {
    match availability {
        ImageAvailability::Present { dimensions, .. } => preview_size(*dimensions),
        _ => (
            IMAGE_PREVIEW_FALLBACK_WIDTH,
            IMAGE_ROW_HEIGHT * IMAGE_PLACEHOLDER_ROWS as f32,
        ),
    }
}

fn path_label(path: &Path) -> SharedString {
    path.to_string_lossy().into_owned().into()
}

fn placeholder(title: impl Into<SharedString>, detail: Option<SharedString>) -> AnyElement {
    div()
        .size_full()
        .min_w_0()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_1()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(IconName::Image)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(title).size(LabelSize::Small).color(Color::Muted)),
        )
        .when_some(detail, |this, detail| {
            this.child(
                div().max_w_full().px_3().truncate().child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        })
        .into_any_element()
}

impl Render for ImageSurface {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let content = match &self.availability {
            ImageAvailability::Present { path, image, .. } => {
                let unreadable_path = path_label(path);
                gpui::img(image.clone())
                    .size_full()
                    .object_fit(ObjectFit::ScaleDown)
                    .with_loading(|| placeholder("Loading image…", None))
                    .with_fallback(move || {
                        placeholder("Image could not be decoded", Some(unreadable_path.clone()))
                    })
                    .into_any_element()
            }
            ImageAvailability::MissingPath => placeholder("No local image path was provided", None),
            ImageAvailability::Loading(_) => placeholder("Loading image…", None),
            ImageAvailability::Unavailable(path, error) => {
                placeholder(error.clone(), Some(path_label(path)))
            }
        };

        div().size_full().min_w_0().child(
            div()
                .size_full()
                .min_w_0()
                .overflow_hidden()
                .rounded_sm()
                .child(content),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn path_uses_path_then_saved_path_with_legacy_semantics() {
        assert_eq!(
            image_path(&json!({"path": "/tmp/primary.png", "savedPath": "/tmp/fallback.png"})),
            Some(PathBuf::from("/tmp/primary.png"))
        );
        assert_eq!(
            image_path(&json!({"path": 42, "savedPath": "/tmp/fallback.png"})),
            None,
            "a present non-string path does not fall through to savedPath"
        );
        assert_eq!(
            image_path(&json!({"savedPath": "/tmp/fallback.png"})),
            Some(PathBuf::from("/tmp/fallback.png"))
        );
        assert_eq!(image_path(&json!({})), None);
    }

    #[test]
    fn image_snapshots_survive_overwrites_deletion_and_reopening() -> anyhow::Result<()> {
        let root =
            std::env::temp_dir().join(format!("harness-image-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        let path = root.join("same.png");
        let cache = root.join("snapshots");
        let first_key = snapshot_path(&cache, "thread:first-view", &path);
        let second_key = snapshot_path(&cache, "thread:second-view", &path);
        // Equal dimensions and byte counts must not disguise changed pixels.
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255])).save(&path)?;
        let first = load_snapshot(&path, Some(&first_key))?;
        image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 255, 0, 255])).save(&path)?;
        let second = load_snapshot(&path, Some(&second_key))?;
        assert_ne!(first.id, second.id);
        assert_eq!(load_snapshot(&path, Some(&first_key))?, first);
        fs::remove_file(&path)?;
        assert_eq!(load_snapshot(&path, Some(&first_key))?, first);
        assert_eq!(load_snapshot(&path, Some(&second_key))?, second);
        assert!(load_snapshot(&path, None).is_err());
        assert_ne!(
            first_key,
            snapshot_path(&cache, "another-thread:first-view", &path)
        );
        // A competing window must not overwrite an already published snapshot.
        persist_snapshot(&first_key, &second.bytes)?;
        assert_eq!(read_image(&first_key)?, first);
        fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn lightbox_hit_area_fits_the_picture_not_the_viewport() {
        assert_eq!(lightbox_size((100., 80.), (1000., 700.)), (100., 80.));
        assert_eq!(lightbox_size((2000., 1000.), (1048., 748.)), (1000., 500.));
        assert_eq!(lightbox_size((1000., 2000.), (1048., 748.)), (350., 700.));
    }

    #[test]
    fn viewed_image_preview_hugs_the_bitmap_aspect_ratio() {
        let wide = preview_size(Some((1522, 667)));
        assert_eq!(wide.0, IMAGE_PREVIEW_MAX_WIDTH);
        assert!((wide.1 - 168.284).abs() < 0.001);
        assert_eq!(preview_size(Some((100, 80))), (100., 80.));
        assert_eq!(
            preview_size(None),
            (IMAGE_PREVIEW_FALLBACK_WIDTH, IMAGE_PREVIEW_FALLBACK_HEIGHT)
        );
    }

    #[test]
    fn lifecycle_upserts_images_and_removes_only_obsolete_surfaces() {
        assert_eq!(
            surface_sync_decision(true, false),
            SurfaceSyncDecision::Upsert
        );
        assert_eq!(
            surface_sync_decision(true, true),
            SurfaceSyncDecision::Upsert
        );
        assert_eq!(
            surface_sync_decision(false, true),
            SurfaceSyncDecision::Remove
        );
        assert_eq!(
            surface_sync_decision(false, false),
            SurfaceSyncDecision::Ignore
        );
        let dirty = keys_to_sync(
            ["old-only", "stable"].map(str::to_string),
            ["stable", "new-only"].map(str::to_string),
        );
        assert_eq!(
            dirty,
            ["old-only", "stable", "new-only"]
                .map(str::to_string)
                .into_iter()
                .collect()
        );
    }
}
