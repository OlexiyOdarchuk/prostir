//! Where this build of the app comes from, as an interface: whether the person owns
//! it (checked at launch, where the store asks for that) and where a newer version
//! comes from. A build from a store has that store's (`MetaStore` for the Meta
//! Horizon Store, feature `store`); one from GitHub or for development has
//! [`NoStore`]. Another store is another implementation.

use crate::i18n::Lang;

#[cfg(all(target_os = "android", feature = "store"))]
mod meta;

/// What the ownership check says so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreCheck {
    /// No answer yet.
    Waiting,
    Owned,
    /// Not owned, or the check could not run: what to tell the person before the app
    /// closes.
    NotOwned(&'static str),
}

/// Where a newer version of the app comes from: what to do, and where (shown large).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateHint {
    pub how: &'static str,
    pub place: &'static str,
}

pub trait Store {
    /// The ownership check, asked once per pass of the loop until the app closes.
    fn poll(&mut self, l: Lang) -> StoreCheck;
    /// Where to get a newer version of the app, for the screen that says to update it.
    fn update_hint(&self, l: Lang) -> UpdateHint;
}

/// A build from GitHub, or for development: nothing to check, and new versions are on
/// the releases page.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoStore;

impl Store for NoStore {
    fn poll(&mut self, _: Lang) -> StoreCheck {
        StoreCheck::Owned
    }

    fn update_hint(&self, l: Lang) -> UpdateHint {
        UpdateHint {
            how: l.t("Встановіть новий APK з", "Install the new APK from"),
            place: "github.com/OlexiyOdarchuk/prostir/releases",
        }
    }
}

/// The store of this build, its check started (the Meta Horizon Store wants it within
/// 10 s of launch).
#[cfg(target_os = "android")]
pub fn of_this_build(app: &android_activity::AndroidApp) -> Box<dyn Store> {
    #[cfg(feature = "store")]
    {
        Box::new(meta::MetaStore::start(app))
    }
    #[cfg(not(feature = "store"))]
    {
        let _ = app;
        Box::new(NoStore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_store_has_nothing_to_check() {
        let mut store = NoStore;
        assert_eq!(store.poll(Lang::En), StoreCheck::Owned);
        assert_eq!(
            store.update_hint(Lang::En),
            UpdateHint {
                how: "Install the new APK from",
                place: "github.com/OlexiyOdarchuk/prostir/releases",
            }
        );
        assert_eq!(store.update_hint(Lang::Uk).how, "Встановіть новий APK з");
    }
}
