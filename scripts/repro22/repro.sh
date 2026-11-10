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
. "$HERE/private_daemon.sh"
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
