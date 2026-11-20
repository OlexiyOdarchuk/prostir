//! A fake headset: connects to the PC app like the Quest app does (encryption,
//! pairing, hello), keeps track of what the PC says, and sends what a test asks.
//!
//! Used by the PC app's end-to-end tests (`crates/host/tests/e2e`, against a PC app built
//! with the fake desktop) and by `prostir-probe`, which checks a real PC by hand.

pub mod find;
mod headset;
pub mod nal;

pub use headset::{
    Arrival, ConnectError, FIRST_VOLUME, FakeHeadset, Keys, Stream, View, quest_step,
};
