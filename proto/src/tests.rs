use super::*;

/// A message as it goes on the wire: tag, length, payload.
fn framed(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn host_bytes(msg: &HostMsg) -> Vec<u8> {
    let mut out = Vec::new();
    msg.write(&mut out).unwrap();
    out
}

fn client_bytes(msg: &ClientMsg) -> Vec<u8> {
    let mut out = Vec::new();
    msg.write(&mut out).unwrap();
    out
}

/// The generic encoding the macro gives a message, for comparing the hand-written
/// media path with it.
fn generic_host(msg: &HostMsg) -> Vec<u8> {
    let mut payload = Vec::new();
    msg.put_fields(&mut payload).unwrap();
    framed(msg.wire_tag(), &payload)
}

fn read_host(bytes: &[u8]) -> io::Result<HostMsg> {
    let mut r = bytes;
    let msg = HostMsg::read(&mut r)?;
    assert!(r.is_empty(), "the reader took the whole message");
    Ok(msg)
}

fn read_client(bytes: &[u8]) -> io::Result<ClientMsg> {
    let mut r = bytes;
    let msg = ClientMsg::read(&mut r)?;
    assert!(r.is_empty(), "the reader took the whole message");
    Ok(msg)
}

/// The decoding error inside an io::Error, if it is one.
fn wire_error(e: &io::Error) -> Option<&WireError> {
    e.get_ref()?.downcast_ref()
}

fn settings() -> PcSettings {
    PcSettings {
        quality: 30,
        codec: Codec::H264,
        software: false,
        sharp: true,
    }
}

fn host_samples() -> Vec<HostMsg> {
    vec![
        HostMsg::Welcome {
            version: 2,
            language: "uk".into(),
        },
        HostMsg::Opened,
        HostMsg::StreamStart {
            id: 2,
            width: 2560,
            height: 1440,
            codec: Codec::H265,
            name: "Prostir-3".into(),
        },
        HostMsg::Frame {
            id: 2,
            keyframe: true,
            pts_us: 123_456_789,
            data: vec![0, 0, 0, 1, 0x40],
        },
        HostMsg::StreamEnd { id: 2 },
        HostMsg::PrimaryChanged { id: 1 },
        HostMsg::Order { ids: vec![0, 3, 1] },
        HostMsg::MirrorWanted { enabled: true },
        HostMsg::Error {
            text: "Не вдалося".into(),
        },
        HostMsg::Audio {
            pcm: vec![0, 1, 2, 3],
        },
        HostMsg::AudioWanted { enabled: false },
        HostMsg::MicWanted { enabled: true },
        HostMsg::MicActive { active: true },
        HostMsg::Incompatible {
            protocol: 10,
            app: "0.3.0".into(),
        },
        HostMsg::Recenter,
        HostMsg::Notice {
            text: "Збережено".into(),
        },
        HostMsg::MouseAt {
            id: 1,
            x: 65535,
            y: 300,
        },
        HostMsg::Abilities { bits: 1 },
        HostMsg::SoundFrom { id: Some(2) },
        HostMsg::SoundFrom { id: None },
        HostMsg::PcSettings {
            settings: settings(),
            encoders: encoders::GPU_H264 | encoders::GPU_H265,
        },
        HostMsg::Focus { id: Some(0) },
        HostMsg::Focus { id: None },
        HostMsg::PairingRequired,
        HostMsg::PairMsg { msg: [4; 33] },
        HostMsg::PairResult { tag: Some([1; 32]) },
        HostMsg::PairResult { tag: None },
    ]
}

fn client_samples() -> Vec<ClientMsg> {
    vec![
        ClientMsg::Hello {
            version: 2,
            min: 2,
            app: "0.2.0".into(),
        },
        ClientMsg::Open,
        ClientMsg::RequestKeyframe { id: 7 },
        ClientMsg::FrameShown {
            id: 1,
            pts_us: 3_600_000_000_123,
        },
        ClientMsg::AddMonitor {
            width: 3440,
            height: 1440,
        },
        ClientMsg::RemoveMonitor { id: 2 },
        ClientMsg::SetOrder { ids: vec![2, 0, 1] },
        ClientMsg::SetMirror { enabled: true },
        ClientMsg::SetPrimary { id: 1 },
        ClientMsg::SetAudio { enabled: true },
        ClientMsg::SetMic { enabled: false },
        ClientMsg::Mic {
            pcm: vec![1, 0, 2, 0],
        },
        ClientMsg::SetMonitors {
            sizes: vec![(2560, 1440), (1440, 2560)],
            primary: 1,
        },
        ClientMsg::SetLayout {
            rows: vec![
                LayoutRow {
                    entries: vec![LayoutEntry {
                        id: 3,
                        center: Some(-1280),
                        middle: Some(-1500),
                    }],
                },
                LayoutRow {
                    entries: vec![
                        LayoutEntry {
                            id: 0,
                            center: Some(-2560),
                            middle: Some(0),
                        },
                        LayoutEntry {
                            id: 1,
                            center: None,
                            middle: None,
                        },
                    ],
                },
            ],
        },
        ClientMsg::Presence { worn: true },
        ClientMsg::SetPcSettings {
            settings: PcSettings {
                quality: 50,
                codec: Codec::H265,
                software: true,
                sharp: false,
            },
        },
        ClientMsg::Battery {
            percent: 72,
            charging: true,
        },
        ClientMsg::DetachWindow,
        ClientMsg::WindowHere { id: 1 },
        ClientMsg::Screenshot { id: 0 },
        ClientMsg::FindMouse,
        ClientMsg::SetNotifications { enabled: true },
        ClientMsg::SetSoundFromScreen { enabled: true },
        ClientMsg::MouseTo {
            id: 2,
            x: 1,
            y: 65535,
        },
        ClientMsg::PairBegin {
            headset_shows: true,
            name: "Meta Quest 3".into(),
        },
        ClientMsg::PairMsg { msg: [1; 33] },
        ClientMsg::PairConfirm { tag: [9; 32] },
    ]
}

/// Every message reads back as it was written, and the samples cover every tag.
#[test]
fn every_message_round_trips() {
    let hosts = host_samples();
    let mut tags: Vec<u8> = hosts.iter().map(HostMsg::wire_tag).collect();
    tags.sort_unstable();
    tags.dedup();
    let mut all = HostMsg::TAGS.to_vec();
    all.sort_unstable();
    assert_eq!(tags, all, "a sample for every PC message");
    let mut stream = Vec::new();
    for m in &hosts {
        assert_eq!(&read_host(&host_bytes(m)).unwrap(), m);
        m.write(&mut stream).unwrap();
    }
    let mut r = stream.as_slice();
    for m in &hosts {
        assert_eq!(&HostMsg::read(&mut r).unwrap(), m);
    }

    let clients = client_samples();
    let mut tags: Vec<u8> = clients.iter().map(ClientMsg::wire_tag).collect();
    tags.sort_unstable();
    tags.dedup();
    let mut all = ClientMsg::TAGS.to_vec();
    all.sort_unstable();
    assert_eq!(tags, all, "a sample for every headset message");
    for m in &clients {
        assert_eq!(&read_client(&client_bytes(m)).unwrap(), m);
    }

    let b = encode_beacon(47830, "desk");
    assert_eq!(decode_beacon(&b), Some((47830, "desk".into())));
}

/// The media messages are read and written by hand: the bytes are those the
/// declared fields give.
#[test]
fn media_by_hand_matches_the_declared_fields() {
    let frame = HostMsg::Frame {
        id: 4,
        keyframe: false,
        pts_us: 77,
        data: vec![9; 1000],
    };
    let audio = HostMsg::Audio { pcm: vec![5; 1920] };
    for m in [&frame, &audio] {
        assert_eq!(host_bytes(m), generic_host(m));
        let mut payload = Vec::new();
        m.put_fields(&mut payload).unwrap();
        assert_eq!(&HostMsg::decode(m.wire_tag(), &payload).unwrap(), m);
    }
    assert_eq!(frame.wire_tag(), tag::FRAME);
    assert_eq!(audio.wire_tag(), tag::AUDIO);
    let mic = ClientMsg::Mic { pcm: vec![3; 960] };
    assert_eq!(mic.wire_tag(), tag::MIC);
    let mut payload = Vec::new();
    mic.put_fields(&mut payload).unwrap();
    assert_eq!(client_bytes(&mic), framed(tag::MIC, &payload));
    // A frame whose keyframe byte is not a boolean.
    let mut bad = host_bytes(&frame);
    bad[6] = 2;
    assert!(read_host(&bad).is_err());
    // Shorter than its header.
    assert!(read_host(&framed(tag::FRAME, &[4, 0])).is_err());
}

/// The version handshake keeps the bytes of protocol 1 forever, so every version can
/// tell the user which side to update.
#[test]
fn the_version_handshake_is_frozen() {
    assert_eq!(
        ClientMsg::Hello {
            version: 0,
            min: 0,
            app: String::new()
        }
        .wire_tag(),
        tag::HELLO
    );
    let hello = ClientMsg::Hello {
        version: 1,
        min: 1,
        app: "0.1.1".into(),
    };
    #[rustfmt::skip]
    let hello_bytes = [
        101, 13, 0, 0, 0,
        1, 0, 0, 0,
        1, 0, 0, 0,
        b'0', b'.', b'1', b'.', b'1',
    ];
    assert_eq!(client_bytes(&hello), hello_bytes);
    assert_eq!(read_client(&hello_bytes).unwrap(), hello);
    assert_eq!(
        ClientMsg::read_first(&mut hello_bytes.as_slice()).unwrap(),
        Some(hello)
    );
    // What a headset from before protocol 2 may send first: a request to pair, as
    // protocol 1 wrote it (whether it shows the code, then its name).
    let mut old_pairing = vec![109, 11, 0, 0, 0, 1];
    old_pairing.extend_from_slice(b"Meta Quest");
    let mut r = old_pairing.as_slice();
    assert_eq!(ClientMsg::read_first(&mut r).unwrap(), None);
    assert!(r.is_empty(), "skipped whole");

    let welcome = HostMsg::Welcome {
        version: 2,
        language: "uk".into(),
    };
    let welcome_bytes = [1, 6, 0, 0, 0, 2, 0, 0, 0, b'u', b'k'];
    assert_eq!(host_bytes(&welcome), welcome_bytes);
    assert_eq!(read_host(&welcome_bytes).unwrap(), welcome);

    let incompatible = HostMsg::Incompatible {
        protocol: 2,
        app: "0.2.0".into(),
    };
    #[rustfmt::skip]
    let incompatible_bytes = [
        17, 9, 0, 0, 0,
        2, 0, 0, 0,
        b'0', b'.', b'2', b'.', b'0',
    ];
    assert_eq!(host_bytes(&incompatible), incompatible_bytes);
    assert_eq!(read_host(&incompatible_bytes).unwrap(), incompatible);

    let error = HostMsg::Error {
        text: "Update".into(),
    };
    let error_bytes = [6, 6, 0, 0, 0, b'U', b'p', b'd', b'a', b't', b'e'];
    assert_eq!(host_bytes(&error), error_bytes);
    assert_eq!(read_host(&error_bytes).unwrap(), error);
}

#[test]
fn messages_are_read_strictly() {
    // Bytes left over after a known message.
    let mut longer = host_bytes(&HostMsg::StreamEnd { id: 1 });
    longer[1] = 2;
    longer.push(0);
    assert!(read_host(&longer).is_err());
    // A codec this build does not know, inside the settings.
    let mut payload = Vec::new();
    HostMsg::PcSettings {
        settings: settings(),
        encoders: 1,
    }
    .put_fields(&mut payload)
    .unwrap();
    payload[4] = 9;
    assert!(read_host(&framed(18, &payload)).is_err());
    // A boolean that is neither 0 nor 1.
    assert!(read_client(&framed(115, &[2])).is_err());
    // `Option` with a tag other than 0 or 1.
    assert!(read_host(&framed(16, &[2, 0])).is_err());
    // A name that is not UTF-8.
    let mut payload = vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    payload.extend_from_slice(&[2, 0, 0xff, 0xfe]);
    assert!(read_host(&framed(2, &payload)).is_err());
    // Frozen text is UTF-8 too.
    assert!(read_host(&framed(6, &[0xc3])).is_err());
    // A set of monitors cut off in the middle of a size: one size, its width, half
    // its height.
    assert!(read_client(&framed(116, &[1, 0, 1, 0, 0, 0, 2, 0])).is_err());
}

#[test]
fn lists_cannot_hold_more_than_there_can_be() {
    // 17 ids in an order: one more than a layout may hold.
    let mut payload = vec![17, 0];
    payload.extend(0..17);
    assert!(read_host(&framed(8, &payload)).is_err());
    assert!(read_client(&framed(108, &payload)).is_err());
    // 17 sizes.
    let mut payload = vec![17, 0];
    payload.extend(std::iter::repeat_n(0, 17 * 8));
    payload.push(0);
    assert!(read_client(&framed(116, &payload)).is_err());
    // 100 empty rows: small on the wire, but not a layout.
    let mut payload = vec![100, 0];
    payload.extend(std::iter::repeat_n(0, 200));
    assert!(read_client(&framed(117, &payload)).is_err());
    // Each row within its limit, but 18 monitors in all.
    let entry = LayoutEntry {
        id: 0,
        center: None,
        middle: None,
    };
    let row = LayoutRow {
        entries: vec![entry; 9],
    };
    let mut payload = Vec::new();
    wire::Limited::put(&vec![row.clone(), row], &mut payload, 16).unwrap();
    assert!(read_client(&framed(117, &payload)).is_err());
    // A monitor in two places.
    let entry = |id| LayoutEntry {
        id,
        center: None,
        middle: None,
    };
    let twice = ClientMsg::SetLayout {
        rows: vec![
            LayoutRow {
                entries: vec![entry(0), entry(1)],
            },
            LayoutRow {
                entries: vec![entry(1)],
            },
        ],
    };
    let err = read_client(&client_bytes(&twice)).unwrap_err();
    assert_eq!(wire_error(&err), Some(&WireError::Repeated(1)));
    // A name longer than a name may be.
    let mut payload = vec![1, 65, 0];
    payload.extend(std::iter::repeat_n(b'a', 65));
    assert!(read_client(&framed(109, &payload)).is_err());
}

/// A message over its limits is not sent: an error for this connection, not a crash.
#[test]
fn a_message_over_its_limits_is_not_sent() {
    let order = HostMsg::Order {
        ids: (0..17).collect(),
    };
    let mut out = Vec::new();
    let err = order.write(&mut out).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
        wire_error(&err),
        Some(&WireError::TooLong { len: 17, max: 16 })
    );
    assert!(out.is_empty(), "nothing half written");
    let name = ClientMsg::PairBegin {
        headset_shows: true,
        name: "x".repeat(65),
    };
    let err = name.write(&mut out).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn a_headset_message_cannot_be_huge() {
    let mut big = vec![117];
    big.extend_from_slice(&(1u32 << 20).to_le_bytes());
    big.extend(std::iter::repeat_n(0u8, 1 << 20));
    assert!(ClientMsg::read(&mut big.as_slice()).is_err());
    // Not even one this build would skip.
    big[0] = 250;
    assert!(ClientMsg::read(&mut big.as_slice()).is_err());
}

/// A message from a newer peer is skipped and the next one still reads.
#[test]
fn skips_unknown_messages() {
    let mut buf = framed(250, &[1, 2, 3]);
    HostMsg::Focus { id: Some(1) }.write(&mut buf).unwrap();
    let mut r = buf.as_slice();
    assert_eq!(
        HostMsg::read(&mut r).unwrap(),
        HostMsg::Focus { id: Some(1) }
    );
    assert!(r.is_empty());

    let mut buf = framed(251, &[7; 10]);
    ClientMsg::SetMic { enabled: true }.write(&mut buf).unwrap();
    let mut r = buf.as_slice();
    assert_eq!(
        ClientMsg::read(&mut r).unwrap(),
        ClientMsg::SetMic { enabled: true }
    );
    // An unknown message cut off is an error, not a skip past the end.
    assert!(HostMsg::read(&mut framed(250, &[1, 2, 3])[..6].as_ref()).is_err());
}

#[test]
fn version_ranges() {
    assert!(compatible(PROTOCOL_VERSION, MIN_PROTOCOL));
    // A newer peer that still understands us.
    assert!(compatible(PROTOCOL_VERSION + 3, PROTOCOL_VERSION));
    // A newer peer that dropped what we speak.
    assert!(!compatible(PROTOCOL_VERSION + 3, PROTOCOL_VERSION + 1));
    // An older peer than we still support: 0.1.x speaks protocol 1.
    assert!(!compatible(MIN_PROTOCOL - 1, 0));
    assert!(!compatible(1, 1));
}
