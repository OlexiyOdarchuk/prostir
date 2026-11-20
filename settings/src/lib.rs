//! Prostir's settings window (`prostir-settings`): a separate program, so the PC part
//! never loads a toolkit. It keeps no settings of its own: it reads the state of the
//! Prostir running in the background over its local API and asks it for changes.
//!
//! - [`api`]: the state and requests as JSON, and the [`api::Client`] trait;
//!   [`dbus`] talks to the service on the session bus, [`mock`] is one in memory.
//! - [`link`]: the client on its own thread, so the window never waits for it.
//! - [`theme`], [`fonts`]: the look as data; [`portal`]: the desktop's light or dark.
//! - [`app`], [`pages`], [`widgets`], [`map`], [`snap`]: the window itself.
//! - [`instance`]: one window per person.
//!
//! Texts are Fluent files in `lang/`, loaded by `prostir-lang`.

pub mod api;
pub mod app;
pub mod dbus;
pub mod fonts;
pub mod instance;
pub mod launch;
pub mod link;
pub mod map;
pub mod mock;
pub mod pages;
pub mod portal;
pub mod snap;
pub mod theme;
pub mod widgets;

pub use app::{App, Options};

/// A client for when there is no session bus at all: every call says so.
pub struct NoBus(pub String);

impl api::Client for NoBus {
    fn get_state(&mut self) -> Result<api::Snapshot, api::ClientError> {
        Err(api::ClientError::NoBus(self.0.clone()))
    }

    fn call(&mut self, _: &api::Request) -> Result<api::Reply, api::ClientError> {
        Err(api::ClientError::NoBus(self.0.clone()))
    }

    fn watch(&mut self, _: api::Sink) -> Result<(), api::ClientError> {
        Ok(())
    }
}
