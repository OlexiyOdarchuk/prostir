//! Sizes of the outputs Prostir makes. Where they go on the desktop is
//! `prostir_layout::desk`, the same for every desktop.

use crate::platform::ActiveWindow;

/// Smallest window panel, in logical pixels.
const MIN_SIZE: (i32, i32) = (320, 200);

/// Output pixel size for a panel showing `w`: even numbers, at least [`MIN_SIZE`].
pub fn panel_size(w: &ActiveWindow) -> (u32, u32) {
    // At most what an output may be (an odd report from the compositor or a spoofed
    // one must not overflow).
    let px = |logical: i32, min: i32| {
        let p = (f64::from(logical.clamp(min, 8192)) * w.scale.clamp(0.25, 4.0)).round() as u32;
        (p.min(8192) + 1) & !1
    };
    (px(w.width, MIN_SIZE.0), px(w.height, MIN_SIZE.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_size_is_even_and_scaled() {
        let w = ActiveWindow {
            id: String::new(),
            width: 801,
            height: 100,
            scale: 1.5,
            output: None,
        };
        assert_eq!(panel_size(&w), (1202, 300));
    }
}
