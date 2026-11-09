#!/bin/bash
# #23: render the FluidSynth piano fixtures (scales, chords, piece, melody, band) and score the piano
# DSP on them, then measure live-path latency on 44.1 kHz renders. Writes
# ~/.cache/wavelink-piano-fixtures/*.{mid,wav}; prints METRIC lines. fluidsynth runs offline
# (-F file), never to an audio device. PIANO_FIX_DRY=1 turns reverb/chorus off.
set -euo pipefail
cd "$(dirname "$0")/.."
target="${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target-23}"
distrobox enter rust-gtk -- bash -lc "cd '$PWD' && CARGO_TARGET_DIR='$target' \
  nice -n 19 cargo test --release -j 6 piano::tests::fluidsynth_ -- --ignored --nocapture --test-threads 1 2>&1" | grep -a 'METRIC\|test result'
