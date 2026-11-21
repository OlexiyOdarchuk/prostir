//! Just enough H.264 and H.265 parsing to tell whether a keyframe can start a
//! decoder (it must carry the parameter sets and an IDR picture), and to cut an
//! encoded clip into the access units a PC sends one by one.

use std::ops::Range;

use prostir_proto::Codec;

/// The NAL unit types in an Annex-B access unit (`00 00 01` or `00 00 00 01` start
/// codes), in order.
pub fn unit_types(data: &[u8], codec: Codec) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            if let Some(&header) = data.get(i + 3) {
                types.push(match codec {
                    Codec::H264 => header & 0x1f,
                    Codec::H265 => (header >> 1) & 0x3f,
                    // AV1 has no NAL units: no PC sends it yet.
                    Codec::Av1 => 0,
                });
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}

/// Whether `data` is a keyframe a decoder can start from: parameter sets and an IDR
/// picture (what the PC sends with every keyframe).
pub fn starts_decoder(data: &[u8], codec: Codec) -> bool {
    let types = unit_types(data, codec);
    let has = |t: u8| types.contains(&t);
    match codec {
        // SPS, PPS, IDR slice.
        Codec::H264 => has(7) && has(8) && has(5),
        // VPS, SPS, PPS, IDR (with or without leading pictures).
        Codec::H265 => has(32) && has(33) && has(34) && (has(19) || has(20)),
        Codec::Av1 => false,
    }
}

/// Where each NAL unit of an Annex-B byte stream starts (its start code, with the
/// zero byte of a 4-byte one), and its header byte offset.
fn units(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            out.push((start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// What a NAL unit is, for cutting a stream into pictures.
enum Kind {
    /// A slice; `first`: the first of its picture.
    Slice {
        first: bool,
    },
    /// What goes before a picture's slices (parameter sets, delimiters, SEI).
    Prefix,
    Other,
}

fn kind(data: &[u8], header: usize, codec: Codec) -> Kind {
    let byte = |k: usize| data.get(header + k).copied().unwrap_or(0);
    match codec {
        Codec::H264 => match byte(0) & 0x1f {
            // first_mb_in_slice is ue(v): a leading 1 bit is 0, the first slice.
            1..=5 => Kind::Slice {
                first: byte(1) & 0x80 != 0,
            },
            6..=9 | 14..=18 => Kind::Prefix,
            _ => Kind::Other,
        },
        Codec::H265 => match (byte(0) >> 1) & 0x3f {
            // first_slice_segment_in_pic_flag, right after the 2-byte header.
            0..=31 => Kind::Slice {
                first: byte(2) & 0x80 != 0,
            },
            32..=35 | 39 => Kind::Prefix,
            _ => Kind::Other,
        },
        Codec::Av1 => Kind::Other,
    }
}

/// The access units (one picture each, with what goes before it) of an Annex-B byte
/// stream, as byte ranges in order. Bytes before the first start code are left out.
pub fn access_units(data: &[u8], codec: Codec) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    // The unit being collected: where it starts, and whether it has a slice yet.
    let mut current: Option<(usize, bool)> = None;
    for (start, header) in units(data) {
        let new = match kind(data, header, codec) {
            Kind::Slice { first } => match &mut current {
                Some((_, sliced)) if !(first && *sliced) => {
                    *sliced = true;
                    None
                }
                _ => Some(true),
            },
            Kind::Prefix => match current {
                Some((_, false)) => None,
                _ => Some(false),
            },
            Kind::Other => current.is_none().then_some(false),
        };
        if let Some(sliced) = new {
            if let Some((from, _)) = current {
                out.push(from..start);
            }
            current = Some((start, sliced));
        }
    }
    if let Some((from, _)) = current {
        out.push(from..data.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two H.264 pictures, the first a keyframe with its parameter sets and two
    /// slices, the second one slice: two access units.
    #[test]
    fn h264_streams_cut_into_pictures() {
        let stream = [
            0, 0, 0, 1, 0x09, 0xf0, // delimiter
            0, 0, 0, 1, 0x67, 1, // SPS
            0, 0, 1, 0x68, 2, // PPS
            0, 0, 1, 0x65, 0x88, 3, // IDR, first slice
            0, 0, 1, 0x65, 0x40, 4, // IDR, second slice
            0, 0, 0, 1, 0x09, 0xf0, // delimiter
            0, 0, 1, 0x41, 0x9a, 5, // P slice, first
        ];
        let units = access_units(&stream, Codec::H264);
        assert_eq!(units, [0..29, 29..stream.len()]);
        assert!(starts_decoder(&stream[units[0].clone()], Codec::H264));
        assert!(!starts_decoder(&stream[units[1].clone()], Codec::H264));
        // Without delimiters, the first slice of a picture starts it.
        let bare = [
            0, 0, 1, 0x65, 0x88, 1, 0, 0, 1, 0x41, 0x9a, 2, 0, 0, 1, 0x41, 0x9a, 3,
        ];
        assert_eq!(access_units(&bare, Codec::H264), [0..6, 6..12, 12..18]);
    }

    #[test]
    fn h265_streams_cut_into_pictures() {
        let stream = [
            0, 0, 1, 0x40, 1, 9, // VPS
            0, 0, 1, 0x42, 1, 9, // SPS
            0, 0, 1, 0x44, 1, 9, // PPS
            0, 0, 1, 0x26, 1, 0x80, // IDR, first slice
            0, 0, 1, 0x02, 1, 0x80, // trailing picture
            0, 0, 1, 0x02, 1, 0x80, // and another
        ];
        let units = access_units(&stream, Codec::H265);
        assert_eq!(units, [0..24, 24..30, 30..36]);
        assert!(starts_decoder(&stream[units[0].clone()], Codec::H265));
    }

    #[test]
    fn h264_keyframes_need_parameter_sets() {
        // SPS, PPS, IDR, with both start code lengths.
        let key = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3];
        assert_eq!(unit_types(&key, Codec::H264), [7, 8, 5]);
        assert!(starts_decoder(&key, Codec::H264));
        // A P-frame slice alone.
        assert!(!starts_decoder(&[0, 0, 1, 0x41, 9], Codec::H264));
    }

    #[test]
    fn h265_keyframes_need_parameter_sets() {
        let key = [
            0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x44, 1, 0, 0, 1, 0x26, 1,
        ];
        assert_eq!(unit_types(&key, Codec::H265), [32, 33, 34, 19]);
        assert!(starts_decoder(&key, Codec::H265));
        assert!(!starts_decoder(&key[..15], Codec::H265));
    }
}
