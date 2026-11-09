# Sourced by repro.sh (adapted from scripts/repro22/private_daemon.sh, #22). Creates a private
# PipeWire+WirePlumber (every hardware monitor off) in a throw-away dir, aborts if any real hardware
# node is visible, and cleans up by PID only (STOP, children, KILL) on EXIT/INT/TERM.
# Sets: D (runtime dir), PIDS, DPIDS (the two daemon PIDs); start_daemon / stop_daemon restart it.
D=$(mktemp -d /tmp/pwi-XXXXXX) || exit 1
[ -n "$D" ] && [ -d "$D" ] || exit 1
chmod 700 "$D"
PIDS=()
DPIDS=()
kill_tree(){ local c; kill -STOP "$1" 2>/dev/null || true; for c in $(pgrep -P "$1" 2>/dev/null || true); do kill_tree "$c"; done; kill -CONT "$1" 2>/dev/null || true; kill "$1" 2>/dev/null || true; }
cleanup(){
  trap - EXIT INT TERM
  for p in "${PIDS[@]:-}" "${DPIDS[@]:-}"; do [ -n "$p" ] && kill_tree "$p"; done
  sleep 1; rm -rf "$D"
}
trap cleanup EXIT INT TERM
unset DBUS_SESSION_BUS_ADDRESS PULSE_SERVER
export XDG_RUNTIME_DIR=$D PIPEWIRE_RUNTIME_DIR=$D XDG_CONFIG_HOME=$D/cfg XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data
mkdir -p "$D/cfg/wireplumber/wireplumber.conf.d" "$D/home"
cat > "$D/cfg/wireplumber/wireplumber.conf.d/99-no-hw.conf" <<'C'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
C
. "$HERE/guard.sh"
# stand-in outputs (made-up names; no real device can match)
mk_sink(){ pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$1 media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null; }
start_daemon(){
  ( HOME=$D/home exec pipewire >>"$D/pw.log" 2>&1 ) & DPIDS+=($!)
  sleep 1
  ( HOME=$D/home exec wireplumber >>"$D/wp.log" 2>&1 ) & DPIDS+=($!)
  sleep 3
  case $(pw-cli info 0 2>&1) in *"Core/4"*) ;; *) echo "private pipewire not up"; exit 1;; esac
  if hw_visible; then echo "ABORT: a real hardware (alsa/bluez/v4l2/raop) node is visible in the private daemon"; exit 1; fi
  for n in test_hw_a1 test_hw_a2 test_hw_a3; do mk_sink "$n"; done
}
stop_daemon(){ local p; for p in "${DPIDS[@]:-}"; do [ -n "$p" ] && kill_tree "$p"; done; for p in "${DPIDS[@]:-}"; do wait "$p" 2>/dev/null || true; done; DPIDS=(); }
start_daemon
