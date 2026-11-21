//! The headset's own files: which there are, and which of them a pass of the main loop
//! changed.
//!
//! The model in memory is what counts from the start on. The files are read once, at
//! the start ([`start`]), and after that only written, off the frame, by the
//! [`Saver`] on a thread of its own; nothing reads them back, so a write that lands
//! later breaks nothing. A pass of the loop only marks what changed ([`Dirty`]); after
//! it each marked file's text is written once into the saver's buffers and handed over
//! (the newest text of a file replaces one still waiting). The texts are written by
//! the parts they belong to: `Settings::write_text`, `layout::Desk::write_text`,
//! `profiles::write_text`, `layout::write_keyboard`.

mod disk;

pub use disk::{Loaded, PersistEvent, Saver, start, write_now};

/// A file the headset keeps in its own folder, in the order they are written when
/// several wait: what is hardest to make again first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum File {
    /// `layout.txt`: the desk arrangement.
    Desk,
    /// `profiles.txt`: the saved setups.
    Profiles,
    /// `keyboard.txt`: where the keyboard window lies.
    Keyboard,
    /// `settings.txt`: what is chosen in the menu.
    Settings,
}

impl File {
    pub const ALL: [File; 4] = [File::Desk, File::Profiles, File::Keyboard, File::Settings];

    /// Its name in the app's folder.
    pub fn name(self) -> &'static str {
        match self {
            File::Settings => "settings.txt",
            File::Desk => "layout.txt",
            File::Profiles => "profiles.txt",
            File::Keyboard => "keyboard.txt",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn bit(self) -> u8 {
        1 << self.index()
    }
}

/// Files, each at most once: those to be written after this pass, however often each
/// changed in it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dirty(u8);

impl Dirty {
    pub fn mark(&mut self, file: File) {
        self.0 |= file.bit();
    }

    pub fn has(self, file: File) -> bool {
        self.0 & file.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The marked files, in the order of [`File::ALL`]; none is marked after.
    pub fn take(&mut self) -> impl Iterator<Item = File> + use<> {
        let taken = std::mem::take(self);
        File::ALL.into_iter().filter(move |f| taken.has(*f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hundred changes of a pass are one write of each file changed.
    #[test]
    fn a_pass_writes_each_file_once() {
        let mut dirty = Dirty::default();
        assert!(dirty.is_empty());
        for i in 0..100 {
            dirty.mark(if i % 2 == 0 {
                File::Desk
            } else {
                File::Settings
            });
        }
        dirty.mark(File::Desk);
        assert!(dirty.has(File::Desk) && !dirty.has(File::Keyboard));
        assert_eq!(
            dirty.take().collect::<Vec<_>>(),
            [File::Desk, File::Settings]
        );
        assert!(dirty.is_empty());
        assert_eq!(dirty.take().count(), 0);
    }

    #[test]
    fn names_are_those_of_before() {
        let names: Vec<&str> = File::ALL.iter().map(|f| f.name()).collect();
        assert_eq!(
            names,
            ["layout.txt", "profiles.txt", "keyboard.txt", "settings.txt"]
        );
    }
}
