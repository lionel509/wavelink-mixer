# wavelink-mixer

Wave Link-style mixer for the PipeWire channel/mix setup.

## What it does

- Voicemeeter-style mixer window (GTK 4 / libadwaita): input strips with gain, 3-band EQ, FX preset and routes to buses, plus bus outputs and an apps panel.
- Routing engine (`wavelink-mixer engine`, run as a systemd user service) that builds and supervises the PipeWire graph from `config.json`.
- Vintage-style cairo instruments: needle gauges, knobs and an RGB spectrum analyser.
- Novation SL MkIII control surface over MIDI (faders, knobs and buttons), read via `aseqdump`.
- Spotify player page using the Web API for browsing and control; audio comes from librespot.
- System monitor: read-only health view of the audio system (CPU, open files, latency, dropouts, services).

## Build

```sh
cargo build --release
```

## Runtime needs

- PipeWire with its command-line tools (`pw-loopback`, `pipewire -c`).
- GTK 4.12+ and libadwaita (the `v4_12` feature of `gtk4` is enabled).
- Optional: `fluidsynth` for the Piano channel synth (used if installed).
- Optional: librespot for Spotify audio, run by `contrib/wavelink-spotify.service` as a systemd user unit that plays into the `ch_spotify` sink.

## Configuration

Config lives at `~/.config/wavelink-mixer/config.json`.

## Latency

- `quantum` in `config.json` (default 256, about 5.3 ms per cycle) is forced on the PipeWire graph with `pw-metadata clock.force-quantum`. It applies live when the engine reloads the config, with no restart; 0 leaves PipeWire's own setting alone.
- `contrib/51-wavelink-usb-latency.conf` sets the Scarlett Solo to period 256 and headroom 128. Install with `mkdir -p ~/.config/wireplumber/wireplumber.conf.d && cp contrib/51-wavelink-usb-latency.conf ~/.config/wireplumber/wireplumber.conf.d/ && systemctl --user restart wireplumber wavelink-engine`. Restarting WirePlumber drops the mixer's loopbacks; restart `wavelink-engine` after it (the install line does both).
- `wavelink-mixer latency [--runs N] [--channel game]` plays clicks through the mixer and reports the delay the mixer adds, plus the device's PipeWire-reported latency.
- The system panel's "ms per cycle" is one graph cycle, not the end-to-end delay; use `latency` for that.
- The force is system-wide (every app, games included) and stays after the engine stops. To release it: set `"quantum": 0` in `config.json` while the engine runs, or run `pw-metadata -n settings 0 clock.force-quantum 0`; it also clears when PipeWire restarts. Valid values: 0 or 32–2048 (numbers may be quoted); other numbers are clamped to that range and rounded up to a power of two, and an unreadable value falls back to 256.
- If `config.json` can't be parsed, the mixer saves a copy as `config.json.bad` (or `config.json.bad.1`, `.2`, … if that name is taken) before writing defaults. If the copy can't be written, the file is left alone and defaults are used in memory only.

## Remote control (LAN)

The engine also serves the mixer on the local network so other computers can view and control it (#26).

- `wavelink-mixer engine` listens on `0.0.0.0:8790` (WebSocket, JSON) and advertises `_wavelink._tcp` with `avahi-publish`.
- Clients must send the token from `~/.config/wavelink-mixer/remote.json` (created on first run, mode 600). `wavelink-mixer remote-token` prints the host, port and token.
- `remote.json` keys: `token`, `enabled`, `bind`, `port`, `mdns`, `caps` (per-bus gain caps in dB, e.g. `{"a1": -6}`; 12 removes a cap). Environment overrides: `WAVELINK_REMOTE_PORT`, `WAVELINK_REMOTE_BIND`, `WAVELINK_REMOTE=0` (off), `WAVELINK_REMOTE_MDNS=0`.
- Safety: network speakers (an AirPlay/RAOP target, or bus `a3`) are capped at -20 dB unless a command sets `unlock`; a physical bus rises at most 6 dB per command (lowering is never limited); unmuting a capped bus above its cap brings it down to the cap first.
- The engine is the writer for remote edits: each command is applied to the current `config.json` under `config.lock`, saved atomically, applied to the live graph with `wpctl`, and broadcast to every client. The mixer window merges instead of overwriting (its own edits win field by field) and picks up remote edits within a second.
- Protocol: the `wavelink-proto` crate (`proto/`). Hello{protocol, token} -> Welcome + full State; Commands get an Ack and every client gets a Delta; Subscribe{meters} streams post-fader peak/RMS levels at about 15 fps (pw-record taps, only while a client wants them).

### Wavelink Remote (client app)

`client/` is a native egui app for macOS, Linux and Windows: discovers engines by mDNS (or type `host:port`), asks for the token once, then shows the channel strips (fader, mute, sends coloured per bus with levels, auto-normalize + target), the bus faders and live meters.

- Build: `cargo build --release -p wavelink-remote` (the workspace's default build only builds the mixer).
- macOS: the `Wavelink Remote (macOS)` GitHub Actions workflow uploads `wavelink-remote-macos-universal.tar.gz` (a universal, ad-hoc signed `Wavelink Remote.app`). It is not notarized: the first time, right-click the app and choose Open (or run `xattr -dr com.apple.quarantine "Wavelink Remote.app"`), and allow Local Network access when macOS asks.
- Headless check: `wavelink-remote --probe HOST[:PORT] TOKEN ['{"op":"set_bus_gain","bus":"a1","db":-20}' ...]` prints the state, sends the commands, prints acks/deltas and a few meter frames.

## Stems models
Downloaded on first use into `~/.local/share/wavelink/models/`, checked against pinned SHA-256.
- Live engine: StemgenRT v0.7 independent streaming HS-TasNet by Axel Delafosse, weights CC BY-NC-SA 4.0 (non-commercial), https://github.com/sweetspotsoundsystem/StemgenRT-5.8. Fetched from upstream, never redistributed by this repo.
- Quality engine: Open-Unmix UMX-HQ (MIT, sigsep/open-unmix-pytorch), ONNX exports in this repo's `models-v1` release, made by `tools/export_models.py`.
- Notes: Basic Pitch `nmp.onnx` by Spotify, Apache-2.0, https://github.com/spotify/basic-pitch. Fetched from upstream.
