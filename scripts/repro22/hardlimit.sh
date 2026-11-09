#!/bin/bash
# Breaker #27: private PipeWire only. Engine at hard==soft 1024 (MODE=hard) or soft-only (MODE=soft); samples expected-node count.
# usage: hardlimit.sh BIN SECS soft|hard
set -u
BIN=$1; SECS=$2; MODE=$3
HERE=$(dirname "$(readlink -f "$0")")
D=$(mktemp -d /tmp/pwi-XXXX); chmod 700 "$D"
unset DBUS_SESSION_BUS_ADDRESS PULSE_SERVER
export XDG_RUNTIME_DIR=$D PIPEWIRE_RUNTIME_DIR=$D XDG_CONFIG_HOME=$D/cfg XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data
mkdir -p "$D/cfg"
( ulimit -Sn 1024; HOME=$D/home; mkdir -p $HOME; exec pipewire >"$D/pw.log" 2>&1 ) & PW=$!
sleep 1
( ulimit -Sn 1024; HOME=$D/home; exec wireplumber >"$D/wp.log" 2>&1 ) & WP=$!
sleep 3
case $(pw-cli info 0 2>&1) in *"Core/4"*) ;; *) echo "private pipewire not up"; kill $PW $WP; exit 1;; esac
for n in alsa_output.usb-Focusrite_Scarlett_Solo_USB_P70NH0J0C8A78C-00.HiFi__Line__sink alsa_output.pci-0000_01_00.1.hdmi-stereo raop_sonos_lionels_room; do
  pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=$n media.class=Audio/Sink object.linger=true audio.position=[FL,FR] }" >/dev/null
done
pw-cli create-node adapter "{ factory.name=support.null-audio-sink node.name=easyeffects_source media.class=Audio/Source/Virtual object.linger=true audio.position=[FL,FR] }" >/dev/null
mkdir -p $D/base; HOME=$D/base "$BIN" preset nosuch - - >/dev/null 2>&1
BASE_CONFIG=$D/base/.config/wavelink-mixer/config.json; export BASE_CONFIG
python3 -I -c "import json,sys;p=sys.argv[1];c=json.load(open(p));c['buses'].append({'id':'a3','name':'Sonos','color':'#fff','kind':{'type':'hardware','target':'raop_sonos_lionels_room'},'gain_db':-30.0,'mute':False});json.dump(c,open(p,'w'))" $BASE_CONFIG
ls -la $BASE_CONFIG || { kill $WP $PW; exit 1; }
export HOME=$D/home-8-5; mkdir -p $HOME
python3 "$HERE/gen_config.py" 8 5 "$HOME"
if [ "$MODE" = hard ]; then L="ulimit -n 1024"; else L="ulimit -Sn 1024"; fi
( eval "$L"; exec "$BIN" engine ) >"$D/engine.log" 2>&1 & EP=$!
kill_tree(){ for c in $(pgrep -P "$1"); do kill_tree "$c"; done; kill "$1" 2>/dev/null; }
t0=$(date +%s); maxchild=0
while [ $(( $(date +%s) - t0 )) -lt "$SECS" ]; do
  for c in $(pgrep -P $EP -x pipewire); do f=$(ls /proc/$c/fd 2>/dev/null | wc -l); [ "$f" -gt "$maxchild" ] && maxchild=$f; lim=$(grep "open files" /proc/$c/limits); done
  c=$(timeout 3 pw-dump 2>/dev/null | grep -oE '"node.name": "(strip_[a-z0-9_]+(\.in)?|route_[a-z0-9_]+|mix_recording.src|voice_chat_mix)"' | grep -v 'route_.*\.in"' | sort -u | wc -l)
  echo "$(( $(date +%s) - t0 )) $c" >> $D/samples
  sleep 0.5
done
echo "samples (t count) where count is max:"; M=$(awk '{print $2}' $D/samples | sort -n | tail -1); echo "max=$M"; awk -v m=$M '$2==m{print $1}' $D/samples | tr '\n' ' '; echo
grep -n 'restarting' $D/engine.log | cut -c1-60
echo "MODE=$MODE restarts=$(grep -c 'restarting graph' $D/engine.log) maxchild=$maxchild childlimit: $lim"
grep -m3 'restarting graph' $D/engine.log | cut -c1-160
grep -ci 'limit\|rlimit\|nofile' $D/engine.log
kill_tree $EP; sleep 1; kill $WP $PW; sleep 1; rm -rf "$D"
