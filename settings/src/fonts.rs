//! Fonts: Noto Sans, the same files the headset menu draws with, or files the theme
//! names. The built-in Noto stays behind any other font for the glyphs it lacks.

use std::sync::Arc;

use eframe::egui::{FontData, FontDefinitions, FontFamily, FontId};

use crate::theme::{Type, expand_home};

static REGULAR: &[u8] = include_bytes!("../../client/assets/NotoSans-Regular.ttf");
static MEDIUM: &[u8] = include_bytes!("../../client/assets/NotoSans-Medium.ttf");

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

fn data(name: &str, fallback: &'static [u8]) -> FontData {
    match name {
        REGULAR_NAME => FontData::from_static(REGULAR),
        MEDIUM_NAME => FontData::from_static(MEDIUM),
        // The theme loader checked the file; if it went away since, the built-in one.
        path => std::fs::read(expand_home(path))
            .map(FontData::from_owned)
            .unwrap_or_else(|_| FontData::from_static(fallback)),
    }
}

/// egui's fonts for the theme's `[type]`.
pub fn definitions(t: &Type) -> FontDefinitions {
    let mut d = FontDefinitions::empty();
    d.font_data
        .insert("regular".into(), Arc::new(data(&t.font, REGULAR)));
    d.font_data
        .insert("medium".into(), Arc::new(data(&t.font_medium, MEDIUM)));
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
