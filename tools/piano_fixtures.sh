#!/bin/bash
# #23: render the FluidSynth piano fixtures (scales, chords, piece) and score the piano DSP on them.
# Writes ~/.cache/wavelink-piano-fixtures/{scales,chords,piece}.{mid,wav}; prints METRIC lines.
# fluidsynth runs offline (-F file), never to an audio device. PIANO_FIX_DRY=1 turns reverb/chorus off.
set -euo pipefail
cd "$(dirname "$0")/.."
distrobox enter rust-gtk -- bash -lc "cd '$PWD' && CARGO_TARGET_DIR=\${CARGO_TARGET_DIR:-$HOME/.cache/cargo-target-23} \
  nice -n 19 cargo test --release -j 6 piano::tests::fluidsynth_fixtures -- --ignored --nocapture 2>&1" | grep -a 'METRIC\|test result'
