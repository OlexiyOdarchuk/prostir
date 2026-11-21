//! What the headset is, as the system says it: only shown and logged (the PC's list
//! of headsets, pairing), never used to decide anything.

use prostir_proto::state::{MAX_SHORT, clip};

/// The headset's name from its maker and model as the system gives them
/// (`Build.MANUFACTURER`, `Build.MODEL`): "Oculus Quest 3", or the model alone when
/// it already names its maker ("Pico 4" from "Pico"). Short enough for the protocol;
/// empty when the system said nothing.
pub fn model_name(manufacturer: &str, model: &str) -> String {
    let (manufacturer, model) = (manufacturer.trim(), model.trim());
    let named = model
        .get(..manufacturer.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(manufacturer));
    let name = if manufacturer.is_empty() || named {
        model.to_owned()
    } else if model.is_empty() {
        manufacturer.to_owned()
    } else {
        format!("{manufacturer} {model}")
    };
    let mut name = clip(&name, MAX_SHORT);
    name.truncate(name.trim_end().len());
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maker_and_model() {
        assert_eq!(model_name("Oculus", "Quest 3"), "Oculus Quest 3");
        assert_eq!(model_name("Pico", "Pico 4"), "Pico 4");
        assert_eq!(model_name("HTC", "htc Vive Focus 3"), "htc Vive Focus 3");
        assert_eq!(model_name(" samsung ", " "), "samsung");
        assert_eq!(model_name("", "Quest 3"), "Quest 3");
        assert_eq!(model_name("", ""), "");
    }

    #[test]
    fn never_longer_than_the_protocol_takes() {
        let long = model_name("A maker with a long name", "and a model with a longer one");
        assert!(long.len() <= MAX_SHORT);
        assert!(long.starts_with("A maker with a long name and"));
        // Cut on a character, not inside one.
        let wide = model_name("Виробник", "Модель з довгою назвою");
        assert!(wide.len() <= MAX_SHORT);
        assert!(wide.starts_with("Виробник Модель"));
    }
}
