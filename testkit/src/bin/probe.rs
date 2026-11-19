//! Desktop test client: connects like the headset would, prints what the PC says and
//! stream stats, and dumps each monitor's elementary stream into
//! `<dir>/monitor-N.{h264,h265}` and the sound into `<dir>/audio.raw`.
//!
//! Usage: prostir-probe [HOST:PORT] [SECONDS] [DIR]
//!
//! `PROBE_SEND=detach|here:N|shot:N|close:N|find|notify|mouse:N` sends one request
//! after two seconds.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use prostir_proto::secure;
use prostir_proto::{ClientMsg, Codec, HostMsg};
use prostir_testkit::{ConnectError, FakeHeadset, Keys};

struct Stats {
    file: BufWriter<std::fs::File>,
    frames: u64,
    keyframes: u64,
    bytes: u64,
    first_is_key: Option<bool>,
}

/// The request named by `PROBE_SEND`.
fn request(what: &str) -> Option<ClientMsg> {
    let num = || {
        what.split(':')
            .nth(1)
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    Some(match what.split(':').next()? {
        "detach" => ClientMsg::DetachWindow,
        "here" => ClientMsg::WindowHere { id: num() },
        "shot" => ClientMsg::Screenshot { id: num() },
        "close" => ClientMsg::RemoveMonitor { id: num() },
        "find" => ClientMsg::FindMouse,
        "notify" => ClientMsg::SetNotifications { enabled: true },
        "mouse" => ClientMsg::MouseTo {
            id: num(),
            x: 32768,
            y: 32768,
        },
        _ => return None,
    })
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args
        .next()
        .unwrap_or_else(|| format!("127.0.0.1:{}", prostir_proto::TCP_PORT));
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    let dir = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let addr = addr
        .to_socket_addrs()?
        .next()
        .with_context(|| format!("no address for {addr}"))?;

    // The probe pairs like a headset, with its own key.
    let keys_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".config/prostir/probe");
    let mut keys = Keys::load(&keys_dir)?;
    // Shows a code here; the PC asks for it in a dialog.
    let mut code = || {
        let code = secure::pairing_code();
        println!("pairing: type {} {} on the PC", &code[..3], &code[3..]);
        code
    };
    let mut headset = match FakeHeadset::connect(addr, &mut keys, Some(&mut code)) {
        Ok(h) => h,
        Err(ConnectError::Incompatible { protocol, app }) => {
            println!("PC speaks protocol v{protocol} (app {app}): update one side");
            return Ok(());
        }
        Err(e) => bail!("{e}"),
    };
    println!(
        "connected, host protocol v{}, language {}",
        headset.version, headset.language
    );

    let mut streams: HashMap<u8, Stats> = HashMap::new();
    // Raw PCM: play with `pw-play --format s16 --rate 48000 --channels 2 audio.raw`.
    let mut audio = BufWriter::new(std::fs::File::create(dir.join("audio.raw"))?);
    let mut audio_bytes = 0u64;
    let start = Instant::now();
    let mut to_send = std::env::var("PROBE_SEND").ok();
    let mut quiet_since = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        if start.elapsed() > Duration::from_secs(2)
            && let Some(what) = to_send.take()
            && let Some(msg) = request(&what)
        {
            println!("sending {msg:?}");
            headset.send(&msg)?;
        }
        let Some(msg) = headset.recv(Duration::from_millis(200))? else {
            if quiet_since.elapsed() > Duration::from_secs(15) {
                bail!("nothing from the PC for 15 s");
            }
            continue;
        };
        quiet_since = Instant::now();
        match msg {
            HostMsg::StreamStart {
                id,
                width,
                height,
                codec,
                name,
            } => {
                println!("stream {id} '{name}': {width}x{height} {codec:?}");
                let ext = if codec == Codec::H265 { "h265" } else { "h264" };
                let file = BufWriter::new(std::fs::File::create(
                    dir.join(format!("monitor-{}.{ext}", id + 1)),
                )?);
                streams.insert(
                    id,
                    Stats {
                        file,
                        frames: 0,
                        keyframes: 0,
                        bytes: 0,
                        first_is_key: None,
                    },
                );
            }
            HostMsg::Frame {
                id, keyframe, data, ..
            } => {
                let Some(s) = streams.get_mut(&id) else {
                    bail!("frame for unannounced stream {id}")
                };
                s.first_is_key.get_or_insert(keyframe);
                s.frames += 1;
                s.keyframes += keyframe as u64;
                s.bytes += data.len() as u64;
                s.file.write_all(&data)?;
            }
            HostMsg::Audio { pcm } => {
                audio_bytes += pcm.len() as u64;
                audio.write_all(&pcm)?;
            }
            HostMsg::StreamEnd { id } => println!("stream {id} ended"),
            HostMsg::PrimaryChanged { id } => println!("primary monitor: {id}"),
            HostMsg::Error { text } => println!("host error: {text}"),
            HostMsg::MirrorWanted { enabled } => println!("mirror wanted: {enabled}"),
            HostMsg::Order { ids } => println!("order: {ids:?}"),
            HostMsg::AudioWanted { enabled } => println!("sound to headset: {enabled}"),
            HostMsg::MicWanted { enabled } => println!("headset microphone wanted: {enabled}"),
            HostMsg::MicActive { active } => println!("PC is recording: {active}"),
            HostMsg::Focus { id } => println!("mouse on: {id:?}"),
            HostMsg::Recenter => println!("PC asks to bring the monitors in front"),
            HostMsg::Notice { text } => println!("PC says: {text}"),
            HostMsg::MouseAt { id, x, y } => println!("mouse on {id} at {x},{y}"),
            HostMsg::Abilities { bits } => println!("PC abilities: {bits:#b}"),
            HostMsg::SoundFrom { id } => println!("sound from monitor {id:?}"),
            HostMsg::PcSettings { settings, encoders } => {
                println!("PC encodes: {settings:?}, can use {encoders:#05b}")
            }
            HostMsg::Incompatible { .. }
            | HostMsg::PairingRequired
            | HostMsg::PairMsg { .. }
            | HostMsg::PairResult { .. }
            | HostMsg::Welcome { .. }
            | HostMsg::Opened => {}
        }
    }
    let secs = start.elapsed().as_secs_f64();
    audio.flush()?;
    println!(
        "sound: {:.1} s of audio in {secs:.1} s",
        audio_bytes as f64 / (prostir_proto::AUDIO_RATE * prostir_proto::AUDIO_CHANNELS * 2) as f64
    );
    for (id, s) in &mut streams {
        s.file.flush()?;
        println!(
            "stream {id}: {} frames ({:.1} fps), {} keyframes, {:.2} Mbit/s, first frame keyframe: {:?}",
            s.frames,
            s.frames as f64 / secs,
            s.keyframes,
            s.bytes as f64 * 8.0 / secs / 1e6,
            s.first_is_key
        );
    }
    Ok(())
}
