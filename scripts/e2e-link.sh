#!/usr/bin/env bash
# How late PC sound reaches the headset while big keyframes go out, on a link shaped
# like a home Wi-Fi network: the end-to-end test `sound_under_keyframes_on_a_shaped_link`
# (the PC app with the fake desktop and a fake headset) inside a private network
# namespace whose loopback `tc netem` limits. Prints the numbers.
#
# Usage: scripts/e2e-link.sh [RATE [DELAY]]     default: 100mbit 2ms
#        scripts/e2e-link.sh wifi
#
# `wifi` is a link like a real Wi-Fi network: its speed changes every 3 s between 40
# and 150 Mbit/s, the delay jitters (3 ms, give or take 1), and the monitors make
# about 55 Mbit/s; the test then checks how the PC paced it.
#
# Needs user namespaces (`unshare -rn`, util-linux), iproute2 `ip` and `tc` with
# netem, and what `cargo e2e` needs (GStreamer with x264enc). It builds with
# optimisations (a debug build cannot encrypt and read this much in time) in
# target/e2e-link, never over target/release; CARGO_TARGET_DIR picks another place.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RATE="${1:-100mbit}"
DELAY="${2:-2ms}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target/e2e-link}"
TEST=(cargo test -q --release -p prostir-host --features fake-desktop --test e2e)

cd "$ROOT"
# Built here: the namespace has no network to fetch anything with.
"${TEST[@]}" --no-run
# Inside: loopback up, the size of a Wi-Fi frame, and the shaped link (both directions
# share it). The queue of 200 packets is about what a Wi-Fi access point holds. The
# fake headset connects to an address of the local network's kind, as over Wi-Fi
# (one of 127.0.0.0/8 would count as USB).
unshare -rn bash -euo pipefail -c '
    ip link set lo up
    ip link set lo mtu 1500
    ip addr add 10.77.0.1/32 dev lo
    if [ "$1" = wifi ]; then
        link=wifi
        tc qdisc add dev lo root netem rate 150mbit delay 3ms 1ms limit 200
        (
            while :; do
                for r in 90 40 120 60 150 70 100 45 130 150; do
                    sleep 3
                    tc qdisc change dev lo root netem rate ${r}mbit delay 3ms 1ms limit 200
                done
            done
        ) &
        trap "kill $!" EXIT
    else
        link="$1 $2"
        tc qdisc add dev lo root netem rate "$1" delay "$2" limit 200
    fi
    shift 2
    PROSTIR_LINK="$link" PROSTIR_LINK_IP=10.77.0.1 "$@" --offline -- --ignored --exact \
        link::sound_under_keyframes_on_a_shaped_link --nocapture
' link "$RATE" "$DELAY" "${TEST[@]}"
