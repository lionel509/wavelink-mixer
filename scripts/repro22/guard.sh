# Sourced by repro.sh and test_guard.sh. Returns 0 when the (private) daemon shows any real-hardware node.
# Captures the output first: `pw-cli | grep -q` under pipefail fails open (SIGPIPE, status 141).
hw_visible() {
  local out
  out=$(pw-cli ls Node 2>/dev/null) || true
  grep -q 'api\.alsa\|alsa_\|api\.bluez5\|bluez_\|api\.v4l2\|v4l2_\|api\.libcamera\|libcamera_\|raop' <<<"$out"
}
