#!/bin/bash
# Breaker #27 round 2: repro.sh's ALSA guard under `set -o pipefail` fails OPEN when grep -q exits
# before pw-cli finishes writing (pw-cli gets SIGPIPE, the pipeline status is 141, `if` is false).
# Uses a fake pw-cli on PATH; touches no PipeWire at all. Exit 1 = guard missed a visible ALSA node.
set -euo pipefail
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
cat > "$T/pw-cli" <<'F'
#!/bin/bash
echo '		node.name = "alsa_output.pci-0000_00_1f.3.analog-stereo"'
for i in $(seq 1 20000); do echo "		node.name = \"n$i\""; done
F
chmod +x "$T/pw-cli"
export PATH=$T:$PATH
# exact guard line from repro.sh
if pw-cli ls Node 2>/dev/null | grep -q 'api.alsa\|alsa_'; then echo "guard OK: caught"; exit 0; fi
echo "guard FAILED OPEN: ALSA node visible but not detected"; exit 1
