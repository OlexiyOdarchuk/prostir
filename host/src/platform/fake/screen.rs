//! The picture of a fake monitor and the GStreamer source the encoder reads it from
//! (`prostirfakescreen screen=NAME`). The source behaves like KWin's outputs streamed
//! through PipeWire, which is what the core has to cope with:
//!
//! - it is live, so encoders work as they do on a real desktop (VA-API keeps frames
//!   back when the source is not live);
//! - started (or started over), it sends the current picture at once;
//! - after that only a changed picture, so an encoder that is paused and resumed
//!   without starting over gets nothing on a still screen;
//! - a change while nobody reads is not queued: the next read gets the newest picture.
//!
//! Every start is logged (`fake screen NAME: started`), so a test sees how often the
//! core started a stream over.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Result;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use gstreamer_base as gst_base;
use gstreamer_base::prelude::*;
use gstreamer_base::subclass::prelude::*;
use gstreamer_video as gst_video;

/// The GStreamer element name of the source.
pub const ELEMENT: &str = "prostirfakescreen";

/// Screens by output name, for the sources to find theirs.
static SCREENS: LazyLock<Mutex<HashMap<String, Arc<Screen>>>> = LazyLock::new(Default::default);

/// One fake monitor's picture: grey bands that move with every change.
pub struct Screen {
    name: String,
    info: gst_video::VideoInfo,
    /// How many times the picture changed.
    changes: Mutex<u64>,
    changed: Condvar,
}

impl Screen {
    /// A screen named `name`, findable by its source until [`Screen::remove`].
    pub fn add(name: &str, width: u32, height: u32) -> Result<Arc<Self>> {
        register()?;
        let info = gst_video::VideoInfo::builder(gst_video::VideoFormat::I420, width, height)
            // Frames come when they come, but a nominal rate: with KWin's "variable"
            // (0/1) x264 holds each frame back until the next one arrives, which
            // KWin's keepalive frames hide and a still fake screen would not.
            .fps(gst::Fraction::new(30, 1))
            .build()?;
        let screen = Arc::new(Self {
            name: name.to_owned(),
            info,
            changes: Mutex::new(0),
            changed: Condvar::new(),
        });
        SCREENS
            .lock()
            .unwrap()
            .insert(name.to_owned(), screen.clone());
        Ok(screen)
    }

    /// No source finds it any more.
    pub fn remove(self: &Arc<Self>) {
        let mut screens = SCREENS.lock().unwrap();
        if screens
            .get(&self.name)
            .is_some_and(|s| Arc::ptr_eq(s, self))
        {
            screens.remove(&self.name);
        }
    }

    /// Something on the screen moved.
    pub fn change(&self) {
        *self.changes.lock().unwrap() += 1;
        self.changed.notify_all();
    }

    /// The picture after `changes` changes, in I420 as GStreamer lays it out.
    fn picture(&self, changes: u64) -> gst::Buffer {
        let (w, h) = (self.info.width() as usize, self.info.height() as usize);
        let stride = self.info.stride()[0] as usize;
        let mut data = vec![128u8; self.info.size()];
        for (y, row) in data[..stride * h].chunks_exact_mut(stride).enumerate() {
            for (x, px) in row[..w].iter_mut().enumerate() {
                *px = ((x + y) / 8 + changes as usize * 4) as u8;
            }
        }
        gst::Buffer::from_mut_slice(data)
    }
}

/// Registers the source with GStreamer, once.
fn register() -> Result<()> {
    static DONE: Mutex<bool> = Mutex::new(false);
    let mut done = DONE.lock().unwrap();
    if !*done {
        gst::Element::register(None, ELEMENT, gst::Rank::NONE, ScreenSrc::static_type())?;
        *done = true;
    }
    Ok(())
}

glib::wrapper! {
    /// The source element for a [`Screen`].
    pub struct ScreenSrc(ObjectSubclass<imp::ScreenSrc>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object;
}

mod imp {
    use super::*;

    #[derive(Default)]
    struct State {
        screen: Option<Arc<Screen>>,
        /// The picture sent last (its number of changes); `None` right after a start.
        sent: Option<u64>,
        /// Asked to stop waiting (stopping or pausing).
        flushing: bool,
    }

    #[derive(Default)]
    pub struct ScreenSrc {
        /// Which screen, by output name (the `screen` property).
        name: Mutex<String>,
        state: Mutex<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ScreenSrc {
        const NAME: &'static str = "ProstirFakeScreen";
        type Type = super::ScreenSrc;
        type ParentType = gst_base::PushSrc;
    }

    impl ObjectImpl for ScreenSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("screen")
                        .nick("Screen")
                        .blurb("Name of the fake output to stream")
                        .build(),
                ]
            });
            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            if pspec.name() == "screen" {
                *self.name.lock().unwrap() = value
                    .get::<Option<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
            }
        }

        fn property(&self, _id: usize, _pspec: &glib::ParamSpec) -> glib::Value {
            self.name.lock().unwrap().to_value()
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_live(true);
            obj.set_format(gst::Format::Time);
            obj.set_do_timestamp(true);
        }
    }

    impl GstObjectImpl for ScreenSrc {}

    impl ElementImpl for ScreenSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static META: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "Prostir fake screen",
                    "Source/Video",
                    "A fake monitor's picture, sent when it changes (tests only)",
                    "Prostir contributors",
                )
            });
            Some(&*META)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PADS: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst_video::VideoCapsBuilder::new()
                    .format(gst_video::VideoFormat::I420)
                    .build();
                vec![
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &caps,
                    )
                    .expect("a valid pad template"),
                ]
            });
            PADS.as_ref()
        }
    }

    impl BaseSrcImpl for ScreenSrc {
        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let name = self.name.lock().unwrap().clone();
            let screen = SCREENS.lock().unwrap().get(&name).cloned().ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::NotFound, ["no fake screen {name}"])
            })?;
            *self.state.lock().unwrap() = State {
                screen: Some(screen),
                sent: None,
                flushing: false,
            };
            log::info!("fake screen {name}: started");
            Ok(())
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            *self.state.lock().unwrap() = State::default();
            Ok(())
        }

        fn caps(&self, filter: Option<&gst::Caps>) -> Option<gst::Caps> {
            let name = self.name.lock().unwrap().clone();
            let screen = SCREENS.lock().unwrap().get(&name).cloned()?;
            let caps = screen.info.to_caps().ok()?;
            Some(match filter {
                Some(f) => f.intersect_with_mode(&caps, gst::CapsIntersectMode::First),
                None => caps,
            })
        }

        fn is_seekable(&self) -> bool {
            false
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            self.state.lock().unwrap().flushing = true;
            if let Some(screen) = &self.state.lock().unwrap().screen {
                screen.changed.notify_all();
            }
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            self.state.lock().unwrap().flushing = false;
            Ok(())
        }
    }

    impl PushSrcImpl for ScreenSrc {
        /// Waits for a picture the encoder has not had yet.
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<gst_base::subclass::base_src::CreateSuccess, gst::FlowError> {
            let screen = self
                .state
                .lock()
                .unwrap()
                .screen
                .clone()
                .ok_or(gst::FlowError::Flushing)?;
            let mut changes = screen.changes.lock().unwrap();
            loop {
                {
                    let mut state = self.state.lock().unwrap();
                    if state.flushing {
                        return Err(gst::FlowError::Flushing);
                    }
                    if state.sent != Some(*changes) {
                        state.sent = Some(*changes);
                        let now = *changes;
                        drop(state);
                        drop(changes);
                        return Ok(gst_base::subclass::base_src::CreateSuccess::NewBuffer(
                            screen.picture(now),
                        ));
                    }
                }
                // Woken by a change or by `unlock`; the timeout only guards a missed
                // wake-up.
                changes = screen
                    .changed
                    .wait_timeout(changes, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
        }
    }
}
