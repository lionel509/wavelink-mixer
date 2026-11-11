# wavelink-mixer

A Voicemeeter-style mixer for PipeWire (GTK 4 / libadwaita window), plus a routing engine that builds and supervises the PipeWire graph from `config.json`. See README.md for the feature list.

## Layout

Cargo workspace: `.` is the mixer app and engine, `proto/` is `wavelink-proto` (wire types), `client/` is `wavelink-remote` (egui client, own CI in `.github/workflows/remote-client.yml`).

Module map (`src/main.rs` declares them, all plain `mod` lines):

- `main.rs`: CLI entry; subcommands are listed in its header doc.
- `config.rs`: `~/.config/wavelink-mixer/config.json`, shared by the UI and the engine.
- `engine.rs` (+ `engine/`): builds and supervises the PipeWire graph. Runtime code is in `engine.rs`; test modules are `engine/tests.rs`, `engine/supervisor_tests.rs`, `engine/breaker27*.rs`.
- `pw.rs`: thin helpers around the PipeWire command-line tools.
- `ui.rs`: mixer window (input strips, buses, apps panel).
- `player.rs` (+ `player/`): Spotify page. `player/images.rs`, `player/api.rs`, `player/view.rs` hold the impl sections split from `player.rs`.
- `spotify.rs`: minimal Spotify Web API client (PKCE login, token refresh).
- `dash.rs`: cairo instruments (needle gauges, knobs, spectrum).
- `dsp.rs`: STFT/ISTFT, resampling, onsets, envelopes for the stem separators.
- `models.rs`: model files, fetched on first use and checked against a pinned SHA-256.
- `sep.rs`: separators and transcriber (StemgenRT, Open-Unmix, Basic Pitch).
- `stems.rs`, `stemcfg.rs`, `stems_view.rs`: Stems session and its settings.
- `notes.rs`: note data for Stems (timeline, SMF writer, fluidsynth player).
- `midi.rs`: Novation SL MkIII control surface.
- `norm.rs`: auto-normalize (moves a channel's gain toward a target loudness).
- `busvol.rs`: physical output bus gains (the saved gain is the source of truth).
- `macvol.rs`: the Mac's own output volume (CoreAudio), for the remote Mac strip.
- `remote/`: LAN control server; the engine serves mixer state over WebSocket.
- `link.rs`, `console.rs`: client side of the remote protocol; `console.rs` is the `remote` GTK console.
- `latency.rs`, `leak.rs`, `sysmon.rs`: latency probe, own-process leak watch, system health (read-only).
- Test-only: `test_breaker_back.rs`, `breaker_front.rs`, `breaker_busvol.rs`, `demo_midi.rs`.

## Build and test

- macOS: `brew install gtk4 libadwaita` (needed for the GTK window).
- Use one shared target directory: `export CARGO_TARGET_DIR=~/.cache/cargo-target`.
- Iterate with `cargo check -p wavelink-mixer --tests`. Run `cargo test -p wavelink-mixer --no-fail-fast`. Build the app with `cargo build --release` only when you need it to run.
- Never run `cargo clean` with the shared target: it wipes every project's cache. `git worktree remove` plus `git worktree prune` is enough.
- Linux: `cargo build --release` (README.md, Build). Linux-only code (`cfg(target_os = "linux")`) compiles only there.
- Remote crates, as CI runs them: `cargo test --locked -p wavelink-proto -p wavelink-remote`.
- Headless smoke tests (no PipeWire needed):
  - `wavelink-mixer engine-conf` prints the generated PipeWire conf and exits. It is the cheapest "the binary starts" check.
  - `wavelink-mixer fake-engine [PORT]` runs a test engine for the remote console.
  - `wavelink-mixer remote-token [--rotate]` prints the host, port and token a remote client needs.

## MIDI demo

The held-note demo plays a WAV (or a synthetic tone) and compares the before and after behaviour:

```sh
WAVELINK_DEMO_WAV=<path> cargo test --release -p wavelink-mixer demo_held_notes -- --ignored --nocapture
```

The source is `src/demo_midi.rs`, and `WAVELINK_DEMO_WAV` is read there.

## Gotchas

- PipeWire child fd limit (#22, #29): the graph child inherits the soft `RLIMIT_NOFILE`, which is 1024 by default. Once the graph grows, it restarts every few seconds and stays capped. `raise_nofile` and `spawn_graph_child` in `engine.rs` exist for this. Repros are in `scripts/repro22/` and `scripts/repro33/`.
- The Linux gtk4 build cannot be built or tested from macOS (#44). A green macOS `cargo check` says nothing about Linux-only code.
- `cfg(linux)` tests (for example the `nofile` tests in `engine.rs`) skip silently on macOS. A green macOS run does not cover them.
- A known macOS failure exists: `remote::breaker36::r2_auto_bind_excludes_vpn_interfaces_not_named_like_tun_or_wg` depends on the host's network interfaces. A VPN can make it fail, so compare test results against a baseline on the same machine.
- Test names are module paths. Moving a test module renames its tests, so compare pass/fail sets by full name.
- Some modules are `mod X;` with the code in `src/X/` (`engine`, `player`). Their children read the parent's private items through `use super::*;`.
