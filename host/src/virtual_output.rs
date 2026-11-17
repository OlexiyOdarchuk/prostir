//! Virtual monitors created by KWin through `zkde_screencast_unstable_v1`.
//!
//! `stream_virtual_output` makes KWin add a real output (visible in System
//! Settings → Display, windows can be moved onto it) and stream its contents
//! into a PipeWire node. Closing the stream removes the output again.
//!
//! One Wayland connection ([`Screencast`]) serves all monitors, which come and
//! go one by one ([`VirtualOutput`]).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use log::{info, warn};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
};

/// Pointer mode from the protocol: render the cursor into the stream.
const POINTER_EMBEDDED: u32 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorSpec {
    /// Size in pixels of the encoded stream.
    pub width: u32,
    pub height: u32,
    /// KWin output scale (UI scaling on that monitor); 1.0 keeps what KScreen remembers.
    pub scale: f64,
}

impl MonitorSpec {
    pub fn validate(&self) -> Result<()> {
        if !(320..=7680).contains(&self.width)
            || !(240..=4320).contains(&self.height)
            || !(0.5..=4.0).contains(&self.scale)
        {
            bail!(
                "monitor {}x{}@{} is out of range",
                self.width,
                self.height,
                self.scale
            );
        }
        Ok(())
    }
}

impl std::fmt::Display for MonitorSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)?;
        if (self.scale - 1.0).abs() > f64::EPSILON {
            write!(f, "@{}", self.scale)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct StreamState {
    node: Option<u32>,
    failed: Option<String>,
    closed: bool,
}

/// Stream states keyed by monitor number, shared with the event thread.
#[derive(Default)]
struct Shared {
    streams: Mutex<HashMap<u32, StreamState>>,
    changed: Condvar,
    /// Compositor outputs by registry name: proxy and connector name (eDP-1, HDMI-A-1…).
    outputs: Mutex<HashMap<u32, (wl_output::WlOutput, Option<String>)>>,
}

impl Shared {
    fn bind_output(
        &self,
        registry: &wl_registry::WlRegistry,
        name: u32,
        version: u32,
        qh: &QueueHandle<State>,
    ) {
        // wl_output v4 announces the connector name.
        if version < 4 {
            return;
        }
        let output = registry.bind::<wl_output::WlOutput, _, _>(name, 4, qh, name);
        self.outputs.lock().unwrap().insert(name, (output, None));
    }
}

struct State {
    shared: Arc<Shared>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                state.shared.bind_output(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some((output, _)) = state.shared.outputs.lock().unwrap().remove(&name) {
                    output.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.shared.outputs.lock().unwrap().get_mut(global)
        {
            entry.1 = Some(name);
        }
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

impl Dispatch<ZkdeScreencastStreamUnstableV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        number: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut streams = state.shared.streams.lock().unwrap();
        let s = streams.entry(*number).or_default();
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => s.node = Some(node),
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => s.failed = Some(error),
            zkde_screencast_stream_unstable_v1::Event::Closed => {
                warn!("KWin closed virtual output Prostir-{number}");
                s.closed = true;
            }
            _ => {}
        }
        state.shared.changed.notify_all();
    }
}

/// Connection to KWin's screencast interface.
pub struct Screencast {
    conn: Connection,
    screencast: ZkdeScreencastUnstableV1,
    qh: QueueHandle<State>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Screencast {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context(
            "cannot connect to the Wayland compositor (is this a Plasma Wayland session?)",
        )?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let screencast: ZkdeScreencastUnstableV1 = globals
            .bind(&qh, 2..=4, ())
            .context("KWin does not expose zkde_screencast_unstable_v1 to this program")?;
        let shared = Arc::new(Shared::default());
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                shared.bind_output(globals.registry(), g.name, g.version, &qh);
            }
        }
        let mut state = State {
            shared: shared.clone(),
        };
        // Collect the connector names before anyone asks for them.
        queue.roundtrip(&mut state)?;

        // Reads events in the background: replies to new monitors, `closed` notices.
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
                        let n = unsafe { libc::poll(&mut pfd, 1, 100) };
                        if n > 0 && guard.read().is_err() {
                            break;
                        }
                    }
                })?
        };
        Ok(Self {
            conn,
            screencast,
            qh,
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// Connector name of the first real (non-virtual) display, e.g. the laptop panel.
    pub fn physical_output(&self) -> Option<String> {
        let outputs = self.shared.outputs.lock().unwrap();
        let mut names: Vec<&String> = outputs
            .values()
            .filter_map(|(_, name)| name.as_ref())
            .filter(|n| !n.starts_with("Virtual-"))
            .collect();
        names.sort();
        names.first().map(|n| (*n).clone())
    }

    /// Streams an existing display (e.g. the laptop panel) without creating anything.
    /// KWin streams a given display only once per connection, so use a fresh
    /// [`Screencast`] for every mirror.
    pub fn mirror(&self, number: u32, output_name: &str) -> Result<VirtualOutput> {
        let output = self
            .shared
            .outputs
            .lock()
            .unwrap()
            .values()
            .find(|(_, n)| n.as_deref() == Some(output_name))
            .map(|(o, _)| o.clone())
            .with_context(|| format!("display {output_name} is gone"))?;
        self.shared
            .streams
            .lock()
            .unwrap()
            .insert(number, StreamState::default());
        let stream = self
            .screencast
            .stream_output(&output, POINTER_EMBEDDED, &self.qh, number);
        // Normally ~10 ms; never hold up the headset's requests for long.
        let mirror = self.wait_for_node(
            number,
            stream,
            output_name.to_owned(),
            Duration::from_secs(2),
        )?;
        info!("mirroring {output_name}, PipeWire node {}", mirror.node);
        Ok(mirror)
    }

    fn wait_for_node(
        &self,
        number: u32,
        stream: ZkdeScreencastStreamUnstableV1,
        name: String,
        timeout: Duration,
    ) -> Result<VirtualOutput> {
        self.conn.flush()?;
        let deadline = Instant::now() + timeout;
        let mut streams = self.shared.streams.lock().unwrap();
        let node = loop {
            let s = streams.get(&number).expect("stream state");
            if let Some(error) = &s.failed {
                let error = error.clone();
                streams.remove(&number);
                drop(streams);
                stream.close();
                let _ = self.conn.flush();
                bail!("KWin refused {name}: {error}");
            }
            if let Some(node) = s.node {
                break node;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                streams.remove(&number);
                drop(streams);
                stream.close();
                let _ = self.conn.flush();
                bail!("timed out waiting for KWin to stream {name}");
            }
            streams = self.shared.changed.wait_timeout(streams, left).unwrap().0;
        };
        Ok(VirtualOutput {
            number,
            node,
            name,
            stream,
            conn: self.conn.clone(),
            shared: self.shared.clone(),
        })
    }

    /// Creates output `Virtual-Prostir-<number>` and waits for its PipeWire node.
    pub fn create(&self, number: u32, spec: &MonitorSpec) -> Result<VirtualOutput> {
        spec.validate()?;
        self.shared
            .streams
            .lock()
            .unwrap()
            .insert(number, StreamState::default());
        let name = format!("Prostir-{number}");
        // The protocol calls these "logical", but KWin uses them as the output's mode
        // (pixel size); scale is applied separately through KScreen.
        let (w, h) = (spec.width as i32, spec.height as i32);
        let stream = if self.screencast.version() >= 4 {
            let description = format!("Prostir — монітор {number}");
            self.screencast.stream_virtual_output_with_description(
                name,
                description,
                w,
                h,
                1.0,
                POINTER_EMBEDDED,
                &self.qh,
                number,
            )
        } else {
            self.screencast.stream_virtual_output(
                name,
                w,
                h,
                1.0,
                POINTER_EMBEDDED,
                &self.qh,
                number,
            )
        };
        let output = self.wait_for_node(
            number,
            stream,
            format!("Virtual-Prostir-{number}"),
            Duration::from_secs(10),
        )?;
        info!("Prostir-{number}: {spec}, PipeWire node {}", output.node);
        if (spec.scale - 1.0).abs() > f64::EPSILON {
            kscreen(output.output_name(), &format!("scale.{}", spec.scale));
        }
        Ok(output)
    }
}

impl Drop for Screencast {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.screencast.destroy();
        let _ = self.conn.flush();
    }
}

/// One KWin output; dropping it removes the output.
pub struct VirtualOutput {
    pub number: u32,
    pub node: u32,
    /// KScreen/connector name: `Virtual-Prostir-N`, or e.g. `eDP-1` for a mirror.
    name: String,
    stream: ZkdeScreencastStreamUnstableV1,
    conn: Connection,
    shared: Arc<Shared>,
}

impl VirtualOutput {
    /// Name KScreen and System Settings use for this output.
    pub fn output_name(&self) -> &str {
        &self.name
    }

    pub fn closed(&self) -> bool {
        self.shared
            .streams
            .lock()
            .unwrap()
            .get(&self.number)
            .is_none_or(|s| s.closed)
    }

    /// Makes this the primary output, so the Plasma panel and new windows go there.
    pub fn make_primary(&self) {
        kscreen(self.output_name(), "priority.1");
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        self.stream.close();
        let _ = self.conn.flush();
        self.shared.streams.lock().unwrap().remove(&self.number);
        info!("{} stream closed", self.name);
    }
}

/// Logical geometry `(x, y, width, height)` of each enabled output, from KScreen.
pub fn output_geometries() -> HashMap<String, (i32, i32, i32, i32)> {
    let Ok(out) = std::process::Command::new("kscreen-doctor")
        .arg("-o")
        .output()
    else {
        return HashMap::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut name = None;
    let mut map = HashMap::new();
    for raw in text.lines() {
        // Strip ANSI colour codes.
        let mut line = String::new();
        let mut esc = false;
        for c in raw.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (false, c) => line.push(c),
                _ => {}
            }
        }
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Output:") {
            name = rest.split_whitespace().nth(1).map(str::to_owned);
        } else if let (Some(n), Some(rest)) = (&name, line.strip_prefix("Geometry:")) {
            let nums: Vec<i32> = rest
                .split(|c: char| !c.is_ascii_digit() && c != '-')
                .filter_map(|t| t.parse().ok())
                .collect();
            if let [x, y, w, h] = nums[..] {
                map.insert(n.clone(), (x, y, w, h));
            }
        }
    }
    map
}

/// Applies several `output.<name>.<setting>` changes in one kscreen-doctor call, so
/// KDE never sees an in-between (overlapping) layout. Retries while KScreen has not
/// caught up with just-created outputs.
pub fn kscreen_apply(settings: &[String]) {
    if settings.is_empty() {
        return;
    }
    for _ in 0..10 {
        match std::process::Command::new("kscreen-doctor")
            .args(settings)
            .output()
        {
            Ok(out) if out.status.success() => {
                info!("KDE layout: {}", settings.join(" "));
                return;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => {
                warn!("cannot run kscreen-doctor: {e}");
                return;
            }
        }
    }
    warn!("kscreen-doctor did not apply {}", settings.join(" "));
}

/// Applies `output.<name>.<setting>` through kscreen-doctor, retrying while KScreen
/// has not caught up with a just-created output.
fn kscreen(output: &str, setting: &str) {
    let arg = format!("output.{output}.{setting}");
    for _ in 0..10 {
        match std::process::Command::new("kscreen-doctor")
            .arg(&arg)
            .output()
        {
            Ok(out) if out.status.success() => {
                info!("{output}: {setting}");
                return;
            }
            Ok(_) => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => {
                warn!("cannot run kscreen-doctor for {arg}: {e}");
                return;
            }
        }
    }
    warn!("kscreen-doctor did not apply {arg}");
}
