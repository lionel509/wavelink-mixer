#!/bin/bash
# Issue #22 repro: engine restart loop when the child pipewire hits RLIMIT_NOFILE=1024.
# Runs ONLY against a private PipeWire+WirePlumber in a throw-away runtime dir (never the live one),
# with the engine at the default 1024 soft fd limit, for each NxM size; prints restarts and max fds.
# usage: repro.sh path/to/wavelink-mixer [SECS_PER_SIZE=60] ["7 5" "8 5" "10 6" ...]
set -u
BIN=$(readlink -f "$1"); SECS=${2:-60}; shift 2 2>/dev/null || shift $#
SIZES=("$@"); [ ${#SIZES[@]} -eq 0 ] && SIZES=("7 5" "8 5" "10 6")
HERE=$(dirname "$(readlink -f "$0")")
D=$(mktemp -d /tmp/pwi-XXXX); chmod 700 "$D"
unset DBUS_SESSION_BUS_ADDRESS
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
kill_tree(){ for c in $(pgrep -P "$1"); do kill_tree "$c"; done; kill "$1" 2>/dev/null; }
for sz in "${SIZES[@]}"; do
  set -- $sz; NIN=$1; NBUS=$2
  export HOME=$D/home-$NIN-$NBUS; rm -rf "$HOME"; mkdir -p "$HOME"
  python3 "$HERE/gen_config.py" "$NIN" "$NBUS" "$HOME" >/dev/null
  ( ulimit -Sn 1024; exec "$BIN" engine ) >"$D/engine-$NIN-$NBUS.log" 2>&1 & EP=$!
  t0=$(date +%s); maxchild=0
  while [ $(( $(date +%s) - t0 )) -lt "$SECS" ]; do
    for c in $(pgrep -P $EP -x pipewire); do f=$(ls /proc/$c/fd 2>/dev/null | wc -l); [ "$f" -gt "$maxchild" ] && maxchild=$f; done
    sleep 0.5
  done
  echo "$NIN inputs x $NBUS buses: $(grep -c 'restarting graph' "$D/engine-$NIN-$NBUS.log") restarts in ${SECS}s, max child fds $maxchild"
  kill_tree $EP; sleep 1
done
kill $WP $PW; sleep 1; rm -rf "$D"
