//! Desktop test client: connects like the headset would, prints stream stats
//! and dumps each monitor's elementary stream into `<dir>/monitor-N.{h264,h265}`.
//!
//! Usage: prostir-probe [HOST:PORT] [SECONDS] [DIR]

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use prostir_proto::secure::{
    self, Identity, Pairing, SecureReader, SecureWriter, TrustStore, confirmation, tags_match,
};
use prostir_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

/// Shows a code here; the PC asks for it in a dialog.
fn pair(
    r: &mut BufReader<SecureReader>,
    w: &mut SecureWriter,
    identity: &Identity,
    peer: &secure::Peer,
) -> Result<()> {
    let code = secure::pairing_code();
    println!("pairing: type {} {} on the PC", &code[..3], &code[3..]);
    ClientMsg::PairBegin {
        headset_shows: true,
        name: "prostir-probe".into(),
    }
    .write(w)?;
    let (pairing, msg) = Pairing::start(&code);
    ClientMsg::PairMsg { msg }.write(w)?;
    let theirs = loop {
        match HostMsg::read(r)? {
            HostMsg::PairMsg { msg } => break msg,
            HostMsg::PairResult { .. } => bail!("the PC gave up pairing"),
            HostMsg::Error { text } => println!("PC: {text}"),
            other => bail!("unexpected {other:?}"),
        }
    };
    let key = pairing.finish(&theirs)?;
    ClientMsg::PairConfirm {
        tag: confirmation(&key, "headset", peer, &identity.public),
    }
    .write(w)?;
    match HostMsg::read(r)? {
        HostMsg::PairResult { ok: true, tag }
            if tags_match(&tag, &confirmation(&key, "pc", peer, &identity.public)) =>
        {
            println!("paired");
            Ok(())
        }
        HostMsg::PairResult { .. } => bail!("the codes did not match"),
        other => bail!("unexpected {other:?}"),
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args
        .next()
        .unwrap_or_else(|| format!("127.0.0.1:{}", prostir_proto::TCP_PORT));
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    let dir = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));

    let stream = TcpStream::connect(&addr)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    // The probe pairs like a headset, with its own key.
    let dir_cfg = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".config/prostir/probe");
    let identity = Identity::load_or_create(&dir_cfg.join("identity"))?;
    let mut trusted = TrustStore::load(&dir_cfg.join("paired_hosts"));
    let (reader, mut w, peer) = secure::handshake(&stream, &identity, true)?;
    let mut r = BufReader::new(reader);
    if !trusted.contains(&peer.key) {
        pair(&mut r, &mut w, &identity, &peer)?;
        trusted.add(peer.key, &addr)?;
    }
    ClientMsg::Hello {
        version: PROTOCOL_VERSION,
    }
    .write(&mut w)?;
    loop {
        match HostMsg::read(&mut r)? {
            HostMsg::Welcome { version } => {
                println!("connected, host protocol v{version}");
                break;
            }
            HostMsg::PairingRequired => {
                pair(&mut r, &mut w, &identity, &peer)?;
                ClientMsg::Hello {
                    version: PROTOCOL_VERSION,
                }
                .write(&mut w)?;
            }
            other => bail!("unexpected {other:?}"),
        }
    }
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;

    struct Stats {
        file: BufWriter<std::fs::File>,
        frames: u64,
        keyframes: u64,
        bytes: u64,
        first_is_key: Option<bool>,
    }
    let mut streams: HashMap<u8, Stats> = HashMap::new();
    // Raw PCM: play with `pw-play --format s16 --rate 48000 --channels 2 audio.raw`.
    let mut audio = BufWriter::new(std::fs::File::create(dir.join("audio.raw"))?);
    let mut audio_bytes = 0u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        match HostMsg::read(&mut r)? {
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
            HostMsg::StreamEnd { id } => println!("stream {id} ended"),
            HostMsg::PrimaryChanged { id } => println!("primary monitor: {id}"),
            HostMsg::Error { text } => println!("host error: {text}"),
            HostMsg::MirrorWanted { enabled } => println!("mirror wanted: {enabled}"),
            HostMsg::Order { ids } => println!("order: {ids:?}"),
            HostMsg::Audio { pcm } => {
                audio_bytes += pcm.len() as u64;
                audio.write_all(&pcm)?;
            }
            HostMsg::AudioWanted { enabled } => println!("sound to headset: {enabled}"),
            HostMsg::PairingRequired | HostMsg::PairMsg { .. } | HostMsg::PairResult { .. } => {}
            HostMsg::Welcome { .. } => {}
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
