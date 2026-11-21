#!/usr/bin/env bash
# Makes the clips the headset's selftest streams from its fake PC, into
# crates/prostir-client/assets/selftest/. Run once by a developer when the clips need
# to change (they are kept in the repository); needs GStreamer with x264enc and
# x265enc (gst-plugins-ugly and gst-plugins-bad).
#
# Each clip is a moving test pattern with its name and a running clock, a few seconds
# long, one keyframe at its start (the fake PC loops it and goes back to that keyframe
# when the headset asks for one), as an Annex-B byte stream with the parameter sets
# in front of the keyframe. The file name says what it is:
# <codec>-<width>x<height>-<fps>.<codec>.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/crates/prostir-client/assets/selftest"
SECONDS_LONG=2
FPS=60
# Mbit/s: small files; the picture only has to decode, not look good.
KBPS=900

for element in videotestsrc x264enc x265enc h264parse h265parse timeoverlay; do
    gst-inspect-1.0 "$element" >/dev/null 2>&1 || {
        echo "!! GStreamer element $element is missing" >&2
        exit 1
    }
done

mkdir -p "$OUT"
rm -f "$OUT"/*.h264 "$OUT"/*.h265

clip() {
    local codec="$1" w="$2" h="$3"
    local name="$codec-${w}x$h-$FPS.$codec"
    local encode
    case "$codec" in
        h264) encode="x264enc tune=zerolatency speed-preset=medium bitrate=$KBPS key-int-max=100000 bframes=0 \
            ! video/x-h264,profile=high ! h264parse config-interval=-1 \
            ! video/x-h264,stream-format=byte-stream,alignment=au" ;;
        h265) encode="x265enc tune=zerolatency speed-preset=fast bitrate=$KBPS key-int-max=100000 \
            ! video/x-h265,profile=main ! h265parse config-interval=-1 \
            ! video/x-h265,stream-format=byte-stream,alignment=au" ;;
    esac
    echo ">> $name"
    # shellcheck disable=SC2086 # the pipeline is words on purpose
    gst-launch-1.0 -q videotestsrc pattern=smpte horizontal-speed=4 num-buffers=$((SECONDS_LONG * FPS)) \
        ! "video/x-raw,format=I420,width=$w,height=$h,framerate=$FPS/1" \
        ! textoverlay text="Prostir selftest $w x $h ${codec^^}" valignment=center halignment=center font-desc="Sans 48" \
        ! timeoverlay valignment=bottom halignment=right font-desc="Sans 36" \
        ! $encode ! filesink location="$OUT/$name"
}

clip h265 2560 1440
clip h264 1920 1080
clip h264 1280 720
ls -l "$OUT"
