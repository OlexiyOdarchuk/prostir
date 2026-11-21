//! Just enough H.264 and H.265 parsing to tell whether a keyframe can start a
//! decoder: it must carry the parameter sets and an IDR picture.

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

#[cfg(test)]
mod tests {
    use super::*;

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
