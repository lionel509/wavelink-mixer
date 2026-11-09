#!/bin/bash
# Issue #22 repro: engine behaviour when the child pipewire runs near RLIMIT_NOFILE=1024.
# Runs ONLY against a private PipeWire+WirePlumber in a throw-away runtime dir with every hardware
# monitor disabled (alsa/bluez/v4l2/libcamera); never reads or touches the live config or services.
# usage: repro.sh path/to/wavelink-mixer [SECS_PER_SIZE=60] ["7 5" "8 5" "10 6" ...]
# env:   HARD=1  engine and daemons get hard==soft==1024 (the raise cannot apply)
set -euo pipefail
BIN=$(readlink -f "$1"); SECS=${2:-60}; shift $(( $# < 2 ? $# : 2 ))
SIZES=("$@"); [ ${#SIZES[@]} -eq 0 ] && SIZES=("7 5" "8 5" "10 6")
HERE=$(dirname "$(readlink -f "$0")")
D=$(mktemp -d /tmp/pwi-XXXXXX) || exit 1
[ -n "$D" ] && [ -d "$D" ] || exit 1
chmod 700 "$D"
PIDS=()
cleanup(){
  trap - EXIT INT TERM
  for p in "${PIDS[@]:-}"; do [ -n "$p" ] && kill_tree "$p"; done
  sleep 1; rm -rf "$D"
}
kill_tree(){ local c; kill -STOP "$1" 2>/dev/null || true; for c in $(pgrep -P "$1" 2>/dev/null || true); do kill_tree "$c"; done; kill -CONT "$1" 2>/dev/null || true; kill "$1" 2>/dev/null || true; }
trap cleanup EXIT INT TERM
LIM="ulimit -Sn 1024"; [ "${HARD:-0}" = 1 ] && LIM="ulimit -n 1024"
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
( eval "$LIM"; HOME=$D/home exec pipewire >"$D/pw.log" 2>&1 ) & PIDS+=($!)
sleep 1
( eval "$LIM"; HOME=$D/home exec wireplumber >"$D/wp.log" 2>&1 ) & PIDS+=($!)
sleep 3
case $(pw-cli info 0 2>&1) in *"Core/4"*) ;; *) echo "private pipewire not up"; exit 1;; esac
. "$HERE/guard.sh"
if hw_visible; then echo "ABORT: a real hardware (alsa/bluez/v4l2/raop) node is visible in the private daemon"; exit 1; fi
# stand-in outputs (names are made up; the monitors above are off so nothing real can match)
for n in test_hw_a1 test_hw_a2 test_hw_a3; do
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$n media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
done
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=easyeffects_source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
# synthetic base config: the engine's own default, plus a third hardware bus (never the live config)
mkdir -p "$D/base"
HOME=$D/base "$BIN" preset nosuch - - >/dev/null 2>&1 || true
BASE=$D/base/.config/wavelink-mixer/config.json
[ -f "$BASE" ] || { echo "could not generate the synthetic base config"; exit 1; }
python3 -I - "$BASE" <<'P'
import json,sys
p=sys.argv[1]; c=json.load(open(p))
hw=[b for b in c['buses'] if b['kind']['type']=='hardware']
for b,t in zip(hw,['test_hw_a1','test_hw_a2']): b['kind']['target']=t
c['buses'].append({'id':'a3','name':'A3','color':'#fff','kind':{'type':'hardware','target':'test_hw_a3'},'gain_db':-30.0,'mute':False})
json.dump(c,open(p,'w'))
P
echo "size | restarts | fd/buffer errors (pw.log+engine) | max child fds | degraded"
for sz in "${SIZES[@]}"; do
  set -- $sz; NIN=$1; NBUS=$2
  H=$D/home-$NIN-$NBUS; mkdir -p "$H"
  BASE_CONFIG="$BASE" python3 -I "$HERE/gen_config.py" "$NIN" "$NBUS" "$H" >"$D/gen-$NIN-$NBUS.txt" || { echo "gen_config failed"; exit 1; }
  grep -q . "$D/gen-$NIN-$NBUS.txt" || { echo "gen_config printed nothing"; exit 1; }
  N_CFG=$(python3 -I -c "import json,sys;c=json.load(open(sys.argv[1]));print(len(c['inputs']),len(c['buses']))" "$H/.config/wavelink-mixer/config.json")
  [ "$N_CFG" = "$NIN $NBUS" ] || { echo "generated config is $N_CFG, wanted $NIN $NBUS"; exit 1; }
  : > "$D/pw.mark"; PWLINES=$(wc -l < "$D/pw.log")
  ( eval "$LIM"; HOME=$H exec "$BIN" engine ) >"$D/engine-$NIN-$NBUS.log" 2>&1 & EP=$!; PIDS+=($EP)
  t0=$(date +%s); maxchild=0
  while [ $(( $(date +%s) - t0 )) -lt "$SECS" ]; do
    for c in $(pgrep -P $EP -x pipewire || true); do f=$(ls /proc/$c/fd 2>/dev/null | wc -l); [ "$f" -gt "$maxchild" ] && maxchild=$f; done
    sleep 0.5
  done
  E=$D/engine-$NIN-$NBUS.log
  ERRS=$(( $(tail -n +$((PWLINES+1)) "$D/pw.log" | grep -c 'Too many open files\|Buffer allocation failed') + $(grep -c 'Too many open files\|Buffer allocation failed' "$E" || true) ))
  echo "$NIN x $NBUS | $(grep -c 'restarting graph' "$E" || true) restarts in ${SECS}s | $ERRS | $maxchild | $(grep -c '^degraded' "$E" || true)"
  kill_tree $EP; wait $EP 2>/dev/null || true; sleep 1
  unset "PIDS[$(( ${#PIDS[@]} - 1 ))]"  # reaped: never signal this PID again
done
