#!/usr/bin/env bash
# #77 demo, gate and live-step driver. It only talks to the `listen` CLI:
# stdin commands `save PATH`, `stop`, `stats`, `quit`; stderr `status: ...`;
# stdout `saved`/`save ...` and `stats ...` lines.
#
#   scripts/listen-demo.sh <wav|live> <plain|saves> <WAV> <OUTDIR> <N>
#   scripts/listen-demo.sh gate <OUTDIR>
#
# wav  : `listen --wav WAV` (the same stdout contract fed from the file, no grant)
# live : `listen --mute` on the Mac system tap, with WAV played by afplay at low volume
#        (LISTEN_UNMUTED=1 drops --mute). Needs the one-time System Audio Recording grant.
set -u

BIN="${WAVELINK_BIN:-$HOME/worktrees/wavelink-mixer/.target/release/wavelink-mixer}"
PLAY_VOLUME="${PLAY_VOLUME:-0.1}"

field() { # field NAME < text: prints the value of NAME=VALUE on the first match
  sed -n "s/.*[ ]$1=\([^ ]*\).*/\1/p" | head -1
}

last_stats() { # last_stats FILE
  grep '^stats ' "$1" 2>/dev/null | tail -1
}

gate() {
  dir=$1
  for f in "$dir"/stream-*-plain-*.txt; do
    [ -e "$f" ] || continue
    echo "$(basename "$f"): $(last_stats "$f")"
  done
  med() { # med MODE: median cpu_per_min_s over plain runs of MODE
    vals=$(for f in "$dir"/stream-"$1"-plain-*.txt; do
             [ -e "$f" ] && last_stats "$f" | field cpu_per_min_s
           done | sort -n)
    [ -z "$vals" ] && return 0
    echo "$vals" | awk '{ a[NR] = $1 } END {
      if (NR % 2) print a[(NR + 1) / 2]; else print (a[NR / 2] + a[NR / 2 + 1]) / 2 }'
  }
  w=$(med wav)
  l=$(med live)
  echo "median cpu_per_min_s wav=${w:-none} live=${l:-none}"
  if [ -z "$w" ] || [ -z "$l" ]; then
    echo "GATE FAIL: needs plain runs in both wav and live modes"
    return 1
  fi
  if awk -v l="$l" -v w="$w" 'BEGIN { exit !(l <= 1.05 * w) }'; then
    echo "GATE PASS: live $l <= 1.05 x wav $w (capture overhead is the cpu_tap_s share of each stats line)"
  else
    echo "GATE FAIL: live $l > 1.05 x wav $w"
    return 1
  fi
}

# reap PID: wait up to 60 s for PID to exit on its own, then SIGKILL it.
reap() {
  i=0
  while kill -0 "$1" 2>/dev/null && [ "$i" -lt 60 ]; do sleep 1; i=$((i + 1)); done
  if kill -0 "$1" 2>/dev/null; then
    echo "listen did not exit after quit; killing pid $1"
    kill -9 "$1" 2>/dev/null
    return 1
  fi
  return 0
}

run() {
  mode=$1; kind=$2; wav=$3; dir=$4; n=$5
  case "$mode" in wav|live) ;; *) echo "mode must be wav or live" >&2; exit 2 ;; esac
  case "$kind" in plain|saves) ;; *) echo "kind must be plain or saves" >&2; exit 2 ;; esac
  [ -f "$wav" ] || { echo "no such WAV: $wav" >&2; exit 2; }
  [ -x "$BIN" ] || { echo "no binary at $BIN (cargo build --release -p wavelink-mixer)" >&2; exit 2; }
  mkdir -p "$dir" || exit 2
  dir=$(cd "$dir" && pwd)
  wav=$(cd "$(dirname "$wav")" && pwd)/$(basename "$wav")
  tag="$mode-$kind-$n"
  out="$dir/stream-$tag.txt"
  log="$dir/stream-$tag.log"
  fifo="$dir/cmd-$tag.fifo"
  rm -f "$fifo" "$out" "$log"
  mkfifo "$fifo" || exit 2

  if [ "$mode" = wav ]; then
    flags=(--wav "$wav")
  elif [ "${LISTEN_UNMUTED:-0}" = 1 ]; then
    flags=()
  else
    flags=(--mute)
  fi

  before="$dir/audio-before-$tag.txt"
  [ "$mode" = live ] && system_profiler SPAudioDataType > "$before" 2>/dev/null

  pid=""; ap=""
  cleanup() {
    exec 3>&- 2>/dev/null
    [ -n "$ap" ] && kill "$ap" 2>/dev/null
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null
    rm -f "$fifo"
  }
  trap cleanup EXIT

  "$BIN" listen "${flags[@]}" < "$fifo" > "$out" 2> "$log" &
  pid=$!
  exec 3<> "$fifo"   # read-write so opening the fifo never blocks

  # Wait for capture to come up: `status: live`, or in live mode 15 s after `status: loading`.
  start=$(date +%s); loading_at=""
  while :; do
    now=$(date +%s)
    grep -q '^status: live' "$log" 2>/dev/null && break
    if [ -z "$loading_at" ] && grep -q '^status: loading' "$log" 2>/dev/null; then loading_at=$now; fi
    if [ "$mode" = live ] && [ -n "$loading_at" ] && [ $((now - loading_at)) -ge 15 ]; then break; fi
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "listen exited before capture was live; log tail:"; tail -20 "$log"; exit 1
    fi
    if [ $((now - start)) -ge 180 ]; then
      echo "timed out waiting for live; log tail:"; tail -20 "$log"; exit 1
    fi
    sleep 1
  done

  if [ "$mode" = live ]; then
    afplay -v "$PLAY_VOLUME" "$wav" &
    ap=$!
  fi

  if [ "$kind" = plain ]; then
    sleep 33
    echo stop >&3
    echo "save $dir/stream-$tag.mid" >&3
    echo stats >&3
    echo quit >&3
  else
    sleep 10
    echo "save /nonexistent-dir/x.mid" >&3
    echo "save $dir/stream-$tag-a.mid" >&3
    sleep 23
    if [ "$mode" = live ]; then
      now_dev="$dir/audio-now-$tag.txt"
      system_profiler SPAudioDataType > "$now_dev" 2>/dev/null
      echo "visible_device_diff=$(diff "$before" "$now_dev" | grep -c '^[<>]')"
    fi
    echo stop >&3
    echo "save $dir/stream-$tag-b.mid" >&3
    echo stats >&3
    sleep 10
    echo stats >&3
    echo quit >&3
  fi

  exec 3>&-
  reap "$pid"
  [ -n "$ap" ] && kill "$ap" 2>/dev/null
  ap=""
  trap - EXIT
  rm -f "$fifo"

  echo "output: $out"
  children=$(ps -axo command | grep -cE 'wavelink-mixer (mac|wav)-tap')
  echo "children_after_stop=$children"

  peak=$(last_stats "$out" | field peak)
  echo "peak=${peak:-none}"

  if [ "$kind" = saves ]; then
    first_save=$(grep -E '^save failed:' "$out" | head -1)
    a_line=$(grep -F "stream-$tag-a.mid" "$out" | grep '^saved ' | head -1)
    b_line=$(grep -F "stream-$tag-b.mid" "$out" | grep '^saved ' | head -1)
    a_notes=$(echo "$a_line" | field notes)
    a_first=$(echo "$a_line" | field first)
    if [ -n "$first_save" ] && [ "${a_notes:-0}" -gt 0 ] 2>/dev/null \
       && awk -v f="${a_first:-99}" 'BEGIN { exit !(f < 3.0) }'; then
      echo "failed_then_retry=ok"
    else
      echo "failed_then_retry=bad"
    fi
    stems=0
    for s in drums bass other; do
      tot=0
      for line in "$a_line" "$b_line"; do
        v=$(echo "$line" | field "$s")
        tot=$((tot + ${v:-0}))
      done
      [ "$tot" -gt 0 ] && stems=$((stems + 1))
    done
    echo "stems_with_notes=$stems"
    s1=$(grep '^stats ' "$out" | tail -2 | head -1 | field cpu_total_s)
    s2=$(grep '^stats ' "$out" | tail -1 | field cpu_total_s)
    if [ -n "$s1" ] && [ -n "$s2" ]; then
      echo "idle_cpu_10s=$(awk -v a="$s1" -v b="$s2" 'BEGIN { printf "%.3f", b - a }')"
    else
      echo "idle_cpu_10s=none"
    fi
  fi

  if [ "$mode" = live ] && awk -v p="${peak:-0}" 'BEGIN { exit !(p < 0.001) }'; then
    echo "UNTESTED: capture silent, grant System Audio Recording to the terminal app"
    exit 3
  fi
}

case "${1:-}" in
  gate)
    [ $# -eq 2 ] || { echo "usage: $0 gate <OUTDIR>" >&2; exit 2; }
    gate "$2"
    ;;
  wav|live)
    [ $# -eq 5 ] || { echo "usage: $0 <wav|live> <plain|saves> <WAV> <OUTDIR> <N>" >&2; exit 2; }
    run "$1" "$2" "$3" "$4" "$5"
    ;;
  *)
    echo "usage: $0 <wav|live> <plain|saves> <WAV> <OUTDIR> <N> | $0 gate <OUTDIR>" >&2
    exit 2
    ;;
esac
