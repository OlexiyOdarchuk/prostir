//! Desktop test client: connects like the headset would, prints stream stats
//! and dumps each monitor's elementary stream into `<dir>/monitor-N.{h264,h265}`.
//!
//! Usage: questmon-probe [HOST:PORT] [SECONDS] [DIR]

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use qmon_proto::{ClientMsg, Codec, HostMsg, PROTOCOL_VERSION};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args
        .next()
        .unwrap_or_else(|| format!("127.0.0.1:{}", qmon_proto::TCP_PORT));
    let seconds: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(10);
    let dir = std::path::PathBuf::from(args.next().unwrap_or_else(|| ".".into()));

    let stream = TcpStream::connect(&addr)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut w = stream.try_clone()?;
    ClientMsg::Hello {
        version: PROTOCOL_VERSION,
    }
    .write(&mut w)?;
    let mut r = BufReader::new(stream);
    match HostMsg::read(&mut r)? {
        HostMsg::Welcome { version } => println!("connected, host protocol v{version}"),
        other => bail!("unexpected {other:?}"),
    }

    struct Stats {
        file: BufWriter<std::fs::File>,
        frames: u64,
        keyframes: u64,
        bytes: u64,
        first_is_key: Option<bool>,
    }
    let mut streams: HashMap<u8, Stats> = HashMap::new();
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
            HostMsg::Welcome { .. } => {}
        }
    }
    let secs = start.elapsed().as_secs_f64();
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
