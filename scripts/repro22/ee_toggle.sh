#!/bin/bash
# Breaker #27 r3. Private PipeWire only: engine on its default config; easyeffects_source stand-in destroyed/created every PERIOD s.
# usage: ee_toggle.sh BIN SECS PERIOD
set -euo pipefail
BIN=$(readlink -f "$1"); SECS=$2; PERIOD=$3
HERE=$(dirname "$(readlink -f "$0")")
. "$HERE/private_daemon.sh"
( eval "$LIM"; HOME=$D/home exec "$BIN" engine ) >"$D/engine.log" 2>&1 & EP=$!; PIDS+=($EP)
sleep 20
echo "initial: $(grep -c restarting $D/engine.log || true) restarts"
t0=$(date +%s); on=1
while [ $(( $(date +%s) - t0 )) -lt "$SECS" ]; do
  sleep "$PERIOD"
  if [ $on = 1 ]; then id=$(pw-dump | python3 -I -c "import json,sys;print([o['id'] for o in json.load(sys.stdin) if o.get('type')=='PipeWire:Interface:Node' and o['info']['props'].get('node.name')=='easyeffects_source'][0])"); pw-cli destroy $id >/dev/null; on=0
  else mk_ee; on=1; fi
  s=$(pw-dump | grep -c '"node.name": "strip_mic' || true)
  echo "t=$(( $(date +%s) - t0 )) ee=$on strip_mic nodes=$s restarts=$(grep -c restarting $D/engine.log || true) degraded=$(grep -c '^degraded' $D/engine.log || true)"
done
grep -E 'restarting|degraded|warning' $D/engine.log | cut -c1-200 | head || true
