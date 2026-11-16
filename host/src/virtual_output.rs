//! Virtual monitors created by KWin through `zkde_screencast_unstable_v1`.
//!
//! `stream_virtual_output` makes KWin add a real output (visible in System
//! Settings → Display, windows can be moved onto it) and stream its contents
//! into a PipeWire node. Closing the stream removes the output again.

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{info, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
};

/// Pointer mode from the protocol: render the cursor into the stream.
const POINTER_EMBEDDED: u32 = 2;

#[derive(Debug, Clone)]
pub struct MonitorSpec {
    /// Size in pixels of the encoded stream.
    pub width: u32,
    pub height: u32,
    /// KWin output scale (UI scaling on that monitor).
    pub scale: f64,
}

#[derive(Default)]
struct StreamState {
    node: Option<u32>,
    failed: Option<String>,
}

struct State {
    streams: Vec<StreamState>,
    closed: Arc<Vec<AtomicBool>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZkdeScreencastUnstableV1,
        _: <ZkdeScreencastUnstableV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let s = &mut state.streams[*idx];
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => s.node = Some(node),
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => s.failed = Some(error),
            zkde_screencast_stream_unstable_v1::Event::Closed => {
                warn!("KWin closed virtual output #{}", idx + 1);
                state.closed[*idx].store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

pub struct VirtualOutputs {
    conn: Connection,
    screencast: ZkdeScreencastUnstableV1,
    streams: Vec<ZkdeScreencastStreamUnstableV1>,
    /// PipeWire node id per output, same order as the specs.
    pub nodes: Vec<u32>,
    pub closed: Arc<Vec<AtomicBool>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VirtualOutputs {
    pub fn create(specs: &[MonitorSpec]) -> Result<Self> {
        let conn = Connection::connect_to_env().context(
            "cannot connect to the Wayland compositor (is this a Plasma Wayland session?)",
        )?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let screencast: ZkdeScreencastUnstableV1 = globals.bind(&qh, 2..=4, ()).context(
            "KWin does not expose zkde_screencast_unstable_v1 to this program; \
             install the .desktop file from the README so KWin grants access",
        )?;

        let closed: Arc<Vec<AtomicBool>> =
            Arc::new(specs.iter().map(|_| AtomicBool::new(false)).collect());
        let mut state = State {
            streams: specs.iter().map(|_| StreamState::default()).collect(),
            closed: closed.clone(),
        };

        let mut streams = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let name = format!("Quest-{}", i + 1);
            // The protocol calls these "logical", but KWin uses them as the output's mode
            // (pixel size) and applies `scale` on top, which is what we want.
            let (w, h) = (spec.width as i32, spec.height as i32);
            let stream = if screencast.version() >= 4 {
                let description = format!("Quest 3 — монітор {}", i + 1);
                screencast.stream_virtual_output_with_description(
                    name,
                    description,
                    w,
                    h,
                    spec.scale,
                    POINTER_EMBEDDED,
                    &qh,
                    i,
                )
            } else {
                screencast.stream_virtual_output(name, w, h, spec.scale, POINTER_EMBEDDED, &qh, i)
            };
            streams.push(stream);
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            queue.roundtrip(&mut state)?;
            if let Some((i, s)) = state
                .streams
                .iter()
                .enumerate()
                .find(|(_, s)| s.failed.is_some())
            {
                for s in &streams {
                    s.close();
                }
                let _ = conn.flush();
                bail!(
                    "KWin refused virtual output #{}: {}",
                    i + 1,
                    s.failed.as_deref().unwrap_or_default()
                );
            }
            if state.streams.iter().all(|s| s.node.is_some()) {
                break;
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for KWin to create the virtual outputs");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let nodes: Vec<u32> = state.streams.iter().map(|s| s.node.unwrap()).collect();
        info!("virtual outputs created, PipeWire nodes: {nodes:?}");
        for (i, spec) in specs.iter().enumerate() {
            if (spec.scale - 1.0).abs() > f64::EPSILON {
                apply_scale(&format!("Virtual-Quest-{}", i + 1), spec.scale);
            }
        }

        // Keep reading events so `closed` notifications arrive and the socket never fills up.
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("wayland".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if queue.dispatch_pending(&mut state).is_err() || queue.flush().is_err() {
                            break;
                        }
                        let Some(guard) = queue.prepare_read() else {
                            continue;
                        };
                        let mut pfd = libc::pollfd {
                            fd: guard.connection_fd().as_raw_fd(),
                            events: libc::POLLIN,
                            revents: 0,
                        };
                        // SAFETY: a single valid pollfd.
                        let n = unsafe { libc::poll(&mut pfd, 1, 200) };
                        if n > 0 && guard.read().is_err() {
                            break;
                        }
                    }
                })?
        };

        Ok(Self {
            conn,
            screencast,
            streams,
            nodes,
            closed,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for VirtualOutputs {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for s in &self.streams {
            s.close();
        }
        self.screencast.destroy();
        let _ = self.conn.flush();
        info!("virtual outputs removed");
    }
}

/// KWin ignores the scale passed to `stream_virtual_output`, so set it through
/// KScreen like System Settings would.
fn apply_scale(output: &str, scale: f64) {
    let arg = format!("output.{output}.scale.{scale}");
    for _ in 0..5 {
        match std::process::Command::new("kscreen-doctor")
            .arg(&arg)
            .output()
        {
            Ok(out) if out.status.success() => {
                info!("{output}: scale {scale}");
                return;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => {
                warn!("cannot run kscreen-doctor to set scale {scale} on {output}: {e}");
                return;
            }
        }
    }
    warn!("kscreen-doctor did not apply scale {scale} to {output}");
}
