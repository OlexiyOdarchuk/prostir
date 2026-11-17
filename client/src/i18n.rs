//! Interface language. Strings live next to the code that shows them, as
//! `lang.t("український", "English")` pairs.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Uk,
    En,
}

impl Lang {
    pub fn t<'a>(self, uk: &'a str, en: &'a str) -> &'a str {
        match self {
            Lang::Uk => uk,
            Lang::En => en,
        }
    }
}
