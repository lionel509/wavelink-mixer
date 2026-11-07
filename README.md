# wavelink-mixer

Wave Link-style mixer for the PipeWire channel/mix setup.

## What it does

- Voicemeeter-style mixer window (GTK 4 / libadwaita): input strips with gain, 3-band EQ, FX preset and routes to buses, plus bus outputs and an apps panel.
- Routing engine (`wavelink-mixer engine`, run as a systemd user service) that builds and supervises the PipeWire graph from `config.json`.
- Vintage-style cairo instruments: needle gauges, knobs and an RGB spectrum analyser.
- Novation SL MkIII control surface over MIDI (faders, knobs and buttons), read via `aseqdump`.
- Discord call panel: per-person volume and mute for your current voice call (needs your own Discord developer app, credentials stored locally).
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
- `contrib/51-wavelink-usb-latency.conf` sets the Scarlett Solo to period 256 and headroom 128. Copy it to `~/.config/wireplumber/wireplumber.conf.d/` and restart WirePlumber.
- `wavelink-mixer latency [--runs N] [--channel game]` plays clicks through the mixer and reports the delay the mixer adds, plus the device's PipeWire-reported latency.
- The system panel's "ms per cycle" is one graph cycle, not the end-to-end delay; use `latency` for that.
