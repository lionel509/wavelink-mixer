#!/bin/bash
# Breaker #27 r3. Private PipeWire only: engine on its default config; easyeffects_source stand-in destroyed/created every PERIOD s.
# usage: ee_toggle.sh BIN SECS PERIOD
set -u
BIN=$1; SECS=$2; PERIOD=$3
G=$(dirname "$(readlink -f "$0")")/guard.sh
D=$(mktemp -d /tmp/pwi-XXXXXX); chmod 700 "$D"; PIDS=()
kill_tree(){ local c; for c in $(pgrep -P "$1" 2>/dev/null); do kill_tree "$c"; done; kill "$1" 2>/dev/null; }
cleanup(){ for p in "${PIDS[@]}"; do kill_tree "$p"; done; sleep 1; rm -rf "$D"; }
trap cleanup EXIT
unset DBUS_SESSION_BUS_ADDRESS PULSE_SERVER
export XDG_RUNTIME_DIR=$D PIPEWIRE_RUNTIME_DIR=$D XDG_CONFIG_HOME=$D/cfg XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data
mkdir -p "$D/cfg/wireplumber/wireplumber.conf.d" "$D/home"
printf 'wireplumber.profiles = { main = { monitor.alsa = disabled\n monitor.alsa-midi = disabled\n monitor.bluez = disabled\n monitor.bluez-midi = disabled\n monitor.v4l2 = disabled\n monitor.libcamera = disabled } }\n' > "$D/cfg/wireplumber/wireplumber.conf.d/99-no-hw.conf"
( ulimit -Sn 1024; HOME=$D/home exec pipewire >"$D/pw.log" 2>&1 ) & PIDS+=($!)
sleep 1
( ulimit -Sn 1024; HOME=$D/home exec wireplumber >"$D/wp.log" 2>&1 ) & PIDS+=($!)
sleep 3
case $(pw-cli info 0 2>&1) in *"Core/4"*) ;; *) echo "private pipewire not up"; exit 1;; esac
. "$G"; if hw_visible; then echo ABORT hw; exit 1; fi
mk(){ pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=easyeffects_source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null; }
mk
( ulimit -Sn 1024; HOME=$D/home exec "$BIN" engine ) >"$D/engine.log" 2>&1 & EP=$!; PIDS+=($EP)
sleep 20
echo "initial: $(grep -c restarting $D/engine.log) restarts"
t0=$(date +%s); on=1
while [ $(( $(date +%s) - t0 )) -lt "$SECS" ]; do
  sleep "$PERIOD"
  if [ $on = 1 ]; then id=$(pw-dump | python3 -I -c "import json,sys;print([o['id'] for o in json.load(sys.stdin) if o.get('type')=='PipeWire:Interface:Node' and o['info']['props'].get('node.name')=='easyeffects_source'][0])"); pw-cli destroy $id >/dev/null; on=0
  else mk; on=1; fi
  s=$(pw-dump | grep -c '"node.name": "strip_mic')
  echo "t=$(( $(date +%s) - t0 )) ee=$on strip_mic nodes=$s restarts=$(grep -c restarting $D/engine.log) degraded=$(grep -c '^degraded' $D/engine.log)"
done
grep -E 'restarting|degraded|warning' $D/engine.log | cut -c1-200 | head
