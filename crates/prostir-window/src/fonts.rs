//! Fonts: Noto Sans, the same files the headset menu draws with, or files the theme
//! names. The built-in Noto stays behind any other font for the glyphs it lacks.

use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions, FontFamily, FontId};

use crate::theme::{FontFiles, Type};

static REGULAR: &[u8] = include_bytes!("../../prostir-client/assets/NotoSans-Regular.ttf");
static MEDIUM: &[u8] = include_bytes!("../../prostir-client/assets/NotoSans-Medium.ttf");

const REGULAR_NAME: &str = "Noto Sans";
const MEDIUM_NAME: &str = "Noto Sans Medium";

/// The family the medium weight is registered under.
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

pub fn font(size: f32, medium_weight: bool) -> FontId {
    FontId::new(
        size,
        if medium_weight {
            medium()
        } else {
            FontFamily::Proportional
        },
    )
}

pub fn is_builtin(name: &str) -> bool {
    name == REGULAR_NAME || name == MEDIUM_NAME
}

/// A file the theme read and checked, or a built-in font. Never a file read here:
/// one that is not a font would stop the window.
fn data(name: &str, file: Option<&crate::theme::FontBytes>, fallback: &'static [u8]) -> FontData {
    match (file, name) {
        (Some(bytes), _) => FontData::from_owned(bytes.0.to_vec()),
        (None, MEDIUM_NAME) => FontData::from_static(MEDIUM),
        (None, REGULAR_NAME) => FontData::from_static(REGULAR),
        (None, _) => FontData::from_static(fallback),
    }
}

/// egui's fonts for the theme's `[type]` and the files it read.
pub fn definitions(t: &Type, files: &FontFiles) -> FontDefinitions {
    let mut d = FontDefinitions::empty();
    d.font_data.insert(
        "regular".into(),
        Arc::new(data(&t.font, files.regular.as_ref(), REGULAR)),
    );
    d.font_data.insert(
        "medium".into(),
        Arc::new(data(&t.font_medium, files.medium.as_ref(), MEDIUM)),
    );
    d.font_data
        .insert("noto".into(), Arc::new(FontData::from_static(REGULAR)));
    d.families.insert(
        FontFamily::Proportional,
        vec!["regular".into(), "noto".into()],
    );
    d.families
        .insert(FontFamily::Monospace, vec!["regular".into(), "noto".into()]);
    d.families
        .insert(medium(), vec!["medium".into(), "noto".into()]);
    d
}
