use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use gpui::{
    AnyElement, Context, IntoElement, ObjectFit, Render, SharedString, StyledImage, Window, div,
    prelude::*,
};
use serde_json::Value;
use ui::{Color, Icon, IconName, IconSize, Label, LabelCommon, LabelSize};

const IMAGE_PREVIEW_ROWS: u32 = 9;
const IMAGE_PLACEHOLDER_ROWS: u32 = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ImageAvailability {
    Present(PathBuf),
    MissingPath,
    MissingFile(PathBuf),
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

pub(crate) fn supplement_key(item_key: &str) -> String {
    format!("image-preview:{item_key}")
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
}

impl ImageSurface {
    pub(crate) fn new(raw: &Value) -> Self {
        Self {
            availability: availability_from_raw(raw),
        }
    }

    pub(crate) fn update(&mut self, raw: &Value, cx: &mut Context<Self>) {
        let availability = availability_from_raw(raw);
        if self.availability != availability {
            self.availability = availability;
            cx.notify();
        }
    }

    pub(crate) fn rows(&self) -> u32 {
        rows_for_availability(&self.availability)
    }
}

fn image_path(raw: &Value) -> Option<PathBuf> {
    raw.pointer("/path")
        .or_else(|| raw.pointer("/savedPath"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
}

fn availability_from_raw(raw: &Value) -> ImageAvailability {
    classify_path(image_path(raw), Path::is_file)
}

fn classify_path(path: Option<PathBuf>, is_file: impl FnOnce(&Path) -> bool) -> ImageAvailability {
    match path {
        Some(path) if is_file(&path) => ImageAvailability::Present(path),
        Some(path) => ImageAvailability::MissingFile(path),
        None => ImageAvailability::MissingPath,
    }
}

fn rows_for_availability(availability: &ImageAvailability) -> u32 {
    match availability {
        ImageAvailability::Present(_) => IMAGE_PREVIEW_ROWS,
        ImageAvailability::MissingPath | ImageAvailability::MissingFile(_) => {
            IMAGE_PLACEHOLDER_ROWS
        }
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
            ImageAvailability::Present(path) => {
                let unreadable_path = path_label(path);
                gpui::img(path.clone())
                    .size_full()
                    .object_fit(ObjectFit::ScaleDown)
                    .with_loading(|| placeholder("Loading image…", None))
                    .with_fallback(move || {
                        placeholder("Image could not be decoded", Some(unreadable_path.clone()))
                    })
                    .into_any_element()
            }
            ImageAvailability::MissingPath => placeholder("No local image path was provided", None),
            ImageAvailability::MissingFile(path) => {
                placeholder("Image file is unavailable", Some(path_label(path)))
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
    fn path_availability_distinguishes_present_missing_and_absent() {
        let present = classify_path(Some(PathBuf::from("preview.png")), |_| true);
        assert_eq!(
            present,
            ImageAvailability::Present(PathBuf::from("preview.png"))
        );

        let missing = classify_path(Some(PathBuf::from("missing.png")), |_| false);
        assert_eq!(
            missing,
            ImageAvailability::MissingFile(PathBuf::from("missing.png"))
        );

        assert_eq!(
            classify_path(None, |_| true),
            ImageAvailability::MissingPath
        );
    }

    #[test]
    fn preview_height_is_bounded_and_placeholders_stay_compact() {
        let preview = ImageAvailability::Present(PathBuf::from("preview.png"));
        let missing = ImageAvailability::MissingFile(PathBuf::from("missing.png"));

        assert_eq!(rows_for_availability(&preview), IMAGE_PREVIEW_ROWS);
        assert_eq!(rows_for_availability(&missing), IMAGE_PLACEHOLDER_ROWS);
        assert_eq!(
            rows_for_availability(&ImageAvailability::MissingPath),
            IMAGE_PLACEHOLDER_ROWS
        );
        assert!(IMAGE_PLACEHOLDER_ROWS < IMAGE_PREVIEW_ROWS);
        assert!(IMAGE_PREVIEW_ROWS <= 16);
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
        assert_eq!(supplement_key("image:7"), "image-preview:image:7");

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
