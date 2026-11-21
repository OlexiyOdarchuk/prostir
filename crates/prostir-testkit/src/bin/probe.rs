//! Desktop test client: connects like the headset would, prints what the PC says and
//! stream stats, and dumps each monitor's elementary stream into
//! `<dir>/monitor-N.{h264,h265}` and the sound into `<dir>/audio.raw`.
//!
//! Usage: prostir-probe [HOST:PORT] [SECONDS] [DIR]
//!
//! `PROBE_SEND=detach|here:N|shot:N|close:N|find|mouse:N` sends one request after
//! two seconds.

use std::collections::HashMap;
use std::io::{BufWriter, Write};
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use prostir_proto::request::Request;
use prostir_proto::secure;
use prostir_proto::state::DisplayId;
use prostir_proto::{ClientMsg, Codec, HostMsg};
use prostir_testkit::{ConnectError, FakeHeadset, Keys};

struct Stats {
    file: BufWriter<std::fs::File>,
    frames: u64,
    keyframes: u64,
    bytes: u64,
    first_is_key: Option<bool>,
}

/// What `PROBE_SEND` names: a request, or another message.
enum Send {
    Request(Request),
    Message(ClientMsg),
}

/// The request named by `PROBE_SEND`.
fn request(what: &str) -> Option<Send> {
    let id = || {
        DisplayId(
            what.split(':')
                .nth(1)
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        )
    };
    Some(match what.split(':').next()? {
        "detach" => Send::Request(Request::DetachWindow),
        "here" => Send::Request(Request::WindowHere { id: id() }),
        "shot" => Send::Request(Request::Screenshot { id: id() }),
        "close" => Send::Request(Request::RemoveDisplay { id: id() }),
        "find" => Send::Request(Request::FindMouse),
        "mouse" => Send::Message(ClientMsg::MouseTo {
            id: id(),
            u: 32768,
            v: 32768,
        }),
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
    for part in headset.view().pc.parts() {
        println!("PC: {part:?}");
    }

    let mut streams: HashMap<DisplayId, Stats> = HashMap::new();
    // Raw PCM: play with `pw-play --format s16 --rate 48000 --channels 2 audio.raw`.
    let mut audio = BufWriter::new(std::fs::File::create(dir.join("audio.raw"))?);
    let mut audio_bytes = 0u64;
    let start = Instant::now();
    let mut to_send = std::env::var("PROBE_SEND").ok();
    let mut quiet_since = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        if start.elapsed() > Duration::from_secs(2)
            && let Some(what) = to_send.take()
        {
            match request(&what) {
                Some(Send::Request(r)) => {
                    let req = headset.request(r.clone())?;
                    println!("request {}: {r:?}", req.0);
                }
                Some(Send::Message(msg)) => {
                    println!("sending {msg:?}");
                    headset.send(&msg)?;
                }
                None => println!("PROBE_SEND={what}: nothing to send"),
            }
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
                generation,
                w,
                h,
                codec,
            } => {
                let name = headset
                    .view()
                    .display(id)
                    .map_or_else(|| format!("{id}"), |d| d.name.clone());
                println!("stream {} '{name}' {generation:?}: {w}x{h} {codec:?}", id.0);
                let ext = match codec {
                    Codec::H264 => "h264",
                    Codec::H265 => "h265",
                    Codec::Av1 => "av1",
                };
                let file = BufWriter::new(std::fs::File::create(
                    dir.join(format!("display-{}.{ext}", id.0)),
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
                    bail!("frame for unannounced stream {}", id.0)
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
            HostMsg::StreamEnd { id, .. } => println!("stream {} ended", id.0),
            HostMsg::Reply { req, outcome } => println!("reply {}: {outcome:?}", req.0),
            HostMsg::Error { text } => println!("host error: {text}"),
            HostMsg::Problem { error, detail, .. } => {
                println!("problem {}: {detail}", error.code());
            }
            HostMsg::VolumeAsk { asked } => println!("PC asks for volume (answered): {asked:?}"),
            HostMsg::Command { req, asked } => {
                println!("command {} (answered done): {asked:?}", req.0);
            }
            HostMsg::Notice { text } => println!("PC says: {text}"),
            HostMsg::Ping { .. } | HostMsg::Pong { .. } => {}
            HostMsg::Goodbye { reason } => {
                println!(
                    "PC ends the session: {:?}",
                    prostir_proto::PcGoodbye::from_code(reason)
                );
            }
            // A part of the PC's state.
            part => println!("PC: {part:?}"),
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
            "stream {}: {} frames ({:.1} fps), {} keyframes, {:.2} Mbit/s, first frame keyframe: {:?}",
            id.0,
            s.frames,
            s.frames as f64 / secs,
            s.keyframes,
            s.bytes as f64 * 8.0 / secs / 1e6,
            s.first_is_key
        );
    }
    Ok(())
}
