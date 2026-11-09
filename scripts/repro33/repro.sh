#!/bin/bash
# Issue #33 repro: physical bus faders must keep their saved gain when devices come back reset.
# Runs ONLY against a private PipeWire+WirePlumber (null sinks test_hw_a1..a3, every hardware monitor
# off) and a scratch HOME; the GUI runs with MIXER_NO_AUDIO=1 on an isolated D-Bus (no portals).
# usage: repro.sh path/to/wavelink-mixer      exit 0 = every scenario passed
set -euo pipefail
BIN=$(readlink -f "$1")
HERE=$(dirname "$(readlink -f "$0")")
. "$HERE/private_daemon.sh"
HOME=$D/home "$BIN" preset nosuch - - >/dev/null 2>&1 || true
CFG=$D/home/.config/wavelink-mixer/config.json
[ -f "$CFG" ] || { echo "no base config"; exit 1; }
python3 -I - "$CFG" <<'P'
import json,sys
p=sys.argv[1]; c=json.load(open(p))
hw=[b for b in c['buses'] if b['kind']['type']=='hardware']
for b,t,g in zip(hw,['test_hw_a1','test_hw_a2'],[-4.2,-15.3]): b['kind']['target']=t; b['gain_db']=g; b['mute']=False
c['buses'].append({'id':'a3','name':'Sonos','color':'#fff','kind':{'type':'hardware','target':'test_hw_a3'},'gain_db':-37.3,'mute':False})
json.dump(c,open(p,'w'),indent=1)
P
nid(){ pw-dump | python3 -I -c "
import json,sys
for o in json.load(sys.stdin):
  if o.get('type')=='PipeWire:Interface:Node' and o['info']['props'].get('node.name')==sys.argv[1]: print(o['id']); break" "$1"; }
# state [a1 a2 a3 expected dB]: prints config vs device per bus; with expectations, fails unless
# config and device both match them (device muted = fail, saved mutes are all off here)
state(){ pw-dump | python3 -I -c "
import json,sys,math
cfg=json.load(open(sys.argv[1])); want=[float(x) for x in sys.argv[2:]]
g={b['kind'].get('target'):(b['id'],b['gain_db'],b['mute']) for b in cfg['buses'] if b['kind']['type']=='hardware'}
dev={}
for o in json.load(sys.stdin):
  if o.get('type')!='PipeWire:Interface:Node': continue
  n=o['info']['props'].get('node.name')
  if n not in g: continue
  for pr in (o['info'].get('params') or {}).get('Props',[]):
    if 'channelVolumes' in pr:
      cv=pr['channelVolumes']; v=(sum(cv)/len(cv))**(1/3); dev[g[n][0]]=(-60.0 if v<=0.1 else 60*math.log10(v), pr.get('mute',False)); break
ok=True; out=[]
for k,(t,(i,gd,gm)) in enumerate(sorted(g.items(), key=lambda x:x[1][0])):
  d,m=dev.get(i,(None,None))
  out.append(f\"{i} cfg={gd:+.1f}{'M' if gm else ''} dev={'-' if d is None else format(d,'+.1f')}{'M' if m else ''}\")
  if want and (abs(gd-want[k])>0.2 or gm or d is None or abs(d-want[k])>0.2 or m): ok=False
print('  '.join(out)+('' if not want else ('  OK' if ok else '  MISMATCH')))
sys.exit(0 if ok else 1)" "$CFG" "$@"; }
setv(){ local id; id=$(nid "$1"); [ -n "$id" ] || { echo "no node $1"; exit 1; }
  case "$1" in test_hw_*) ;; *) echo "refusing $1"; exit 1;; esac
  hw_visible && { echo "ABORT hw visible"; exit 1; }
  wpctl set-volume "$id" "$2"; [ -n "${3:-}" ] && wpctl set-mute "$id" "$3"; true; }
GP=
start_gui(){ ( cd "$D"; HOME=$D/home MIXER_NO_AUDIO=1 GDK_DEBUG=no-portals GTK_A11Y=none GDK_BACKEND=wayland WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-wayland-0} \
  XDG_RUNTIME_DIR=$D exec dbus-run-session --config-file="$HERE/isolated-bus.conf" -- "$BIN" ) >>"$D/gui.log" 2>&1 & GP=$!; PIDS+=($GP); }
stop_gui(){ kill_tree "$GP"; wait "$GP" 2>/dev/null || true; sleep 1; }
# the GUI needs the real Wayland socket while its PipeWire is the private one
case "${WAYLAND_DISPLAY:-wayland-0}" in /*) ;; *) WAYLAND_DISPLAY=/run/user/$(id -u)/${WAYLAND_DISPLAY:-wayland-0};; esac
FAIL=0
check(){ local i; for i in $(seq 1 "$1"); do sleep 1; done; shift; echo -n "  after: "; state "$@" || FAIL=1; }
WANT="-4.2 -15.3 -37.3"

echo "== A: devices at 100% (PipeWire restart default), GUI start"
setv test_hw_a1 1.0; setv test_hw_a2 1.0; setv test_hw_a3 1.0; sleep 0.5; echo "  before: $(state)"
start_gui; check 4 $WANT; stop_gui
echo "== B: Sonos muted at volume 0, GUI restart"
setv test_hw_a3 0 1; sleep 0.5; echo "  before: $(state)"
start_gui; check 4 $WANT; stop_gui
echo "== C: Sonos unmuted at volume 0, GUI restart"
setv test_hw_a3 0 0; sleep 0.5; echo "  before: $(state)"
start_gui; check 4 $WANT
echo "== D: GUI running, Sonos node re-created at 100%"
pw-cli destroy "$(nid test_hw_a3)" >/dev/null; sleep 0.5; rm -rf "$D/state/wireplumber"; mk_sink test_hw_a3; check 4 $WANT
echo "== F: GUI running, PipeWire+WirePlumber restarted (state wiped: every sink back at 100%, same serials)"
stop_daemon; rm -rf "$D/state/wireplumber"; start_daemon; echo "  restarted: $(state)"; check 5 $WANT
echo "== E: GUI running, deliberate external change a2 -> 0.5 (-18.1 dB), held"
setv test_hw_a2 0.5; check 5 -4.2 -18.1 -37.3
stop_gui
echo "== GUI log"; grep '^wavelink' "$D/gui.log" || true
[ $FAIL = 0 ] && echo "PASS" || echo "FAIL"
exit $FAIL
