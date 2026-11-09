#!/bin/bash
# #23 by-ear check: song.wav -> song.piano.mid (lanes split) -> song.piano.wav rendered offline with
# FluidSynth (Acoustic Grand), to listen to next to the original. Plays nothing.
# usage: tools/piano_by_ear.sh song.wav [wavelink-mixer binary]
set -euo pipefail
in="$1"
bin="${2:-${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target-23}/release/wavelink-mixer}"
mid="${in%.*}.piano.mid"
out="${in%.*}.piano.wav"
"$bin" piano --wav "$in" --out "$mid"
fs=/home/linuxbrew/.linuxbrew/bin/fluidsynth
[ -x "$fs" ] || fs=fluidsynth
"$fs" -ni -q -g 0.6 -r 44100 -F "$out" "$HOME/.local/share/wavelink/soundfonts/FluidR3_GM.sf2" "$mid"
echo "listen: $out"
