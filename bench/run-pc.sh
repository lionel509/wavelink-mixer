#!/usr/bin/env bash
# Throwaway bench for issue #20 (not a PR). Runs on the gaming PC (Linux, Bazzite) only when the main session says it is free.
# Does not suspend, wake or sleep the machine.
# GPU_NAME picks the card by name (default RTX 2080 SUPER); SKIP_CPU=1 skips the CPU pass.
# cargo runs under nice -n 19 with -j 6 because the PC is in use for gaming.
set -euo pipefail

REPO_URL="https://github.com/lionel509/wavelink-mixer.git"
WT="$HOME/worktrees/wavelink-mixer/20-bench"
BRANCH="bench/20-sep-speed"
VENV="$HOME/bench20-venv"
LOG="$HOME/bench20-$(date +%Y%m%d-%H%M).log"
GPU_NAME="${GPU_NAME:-RTX 2080 SUPER}"

exec > >(tee -a "$LOG") 2>&1
echo "== bench20 $(date -Iseconds) log=$LOG"

# 1. Checkout of the bench branch.
if [ -d "$WT/.git" ] || [ -f "$WT/.git" ]; then
    git -C "$WT" fetch origin "$BRANCH"
else
    mkdir -p "$(dirname "$WT")"
    git clone --no-checkout "$REPO_URL" "$WT.tmp"
    mv "$WT.tmp" "$WT"
    git -C "$WT" fetch origin "$BRANCH"
fi
git -C "$WT" checkout -B "$BRANCH" "origin/$BRANCH"
cd "$WT"
echo "branch $(git rev-parse --abbrev-ref HEAD) @ $(git rev-parse --short HEAD)"

# 2. GPUs.
echo "== nvidia-smi -L"
nvidia-smi -L

# 3. CPU run: CUDA hidden, default features (the code as it is today).
if [ "${SKIP_CPU:-0}" != "1" ]; then
    echo "== CPU run"
    CUDA_VISIBLE_DEVICES= nice -n 19 cargo test -j 6 --release bench20 -- --ignored --nocapture
fi

# 4. Venv with the CUDA 12.8+ runtime wheels and cuDNN 9.
if [ ! -x "$VENV/bin/python" ]; then
    python3 -m venv "$VENV"
fi
"$VENV/bin/pip" install --upgrade pip
"$VENV/bin/pip" install --upgrade onnxruntime-gpu \
    "nvidia-cuda-runtime-cu12>=12.8" "nvidia-cublas-cu12>=12.8" nvidia-cufft-cu12 nvidia-curand-cu12 \
    "nvidia-cuda-nvrtc-cu12>=12.8" "nvidia-cudnn-cu12>=9,<10"

# 5. Library paths: every nvidia/*/lib dir, and the ORT shared library from the wheel.
SP="$("$VENV/bin/python" -c 'import site; print(site.getsitepackages()[0])')"
LD_LIBRARY_PATH="$(find "$SP/nvidia" -maxdepth 2 -type d -name lib | paste -sd: -)${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export LD_LIBRARY_PATH
ORT_SO="$(ls "$SP"/onnxruntime/capi/libonnxruntime.so* | head -n1)"
export ORT_DYLIB_PATH="$ORT_SO"
echo "ORT_DYLIB_PATH=$ORT_DYLIB_PATH"
echo "== ORT version: $("$VENV/bin/python" -c 'import onnxruntime as o; print(o.__version__)')"
echo "== CUDA runtime version: $("$VENV/bin/python" -c 'import importlib.metadata as m; print(m.version("nvidia-cuda-runtime-cu12"))')"
echo "== CUDA providers in wheel: $("$VENV/bin/python" -c 'import onnxruntime as o; print(o.get_available_providers())')"

# 6. Pick the 5070 Ti by UUID (never a bare index).
MATCHES="$(nvidia-smi -L | grep -F "$GPU_NAME" || true)"
N_MATCH="$(printf '%s' "$MATCHES" | grep -c . || true)"
if [ "$N_MATCH" -ne 1 ]; then
    echo "expected exactly one GPU matching '$GPU_NAME' in nvidia-smi -L, found $N_MATCH; aborting"
    exit 1
fi
UUID="$(printf '%s\n' "$MATCHES" | sed -E 's/.*UUID: (GPU-[^)]+)\).*/\1/')"
if [ -z "$UUID" ]; then
    echo "could not parse UUID for '$GPU_NAME'; aborting"
    exit 1
fi
export CUDA_DEVICE_ORDER=PCI_BUS_ID CUDA_VISIBLE_DEVICES="$UUID"
echo "GPU $UUID selected"

# 7. CUDA run with a 1 s sampler of compute apps (stopped by PID).
SAMPLES="$HOME/bench20-smi-$(date +%Y%m%d-%H%M).csv"
(
    while true; do
        nvidia-smi --query-compute-apps=gpu_uuid,pid,process_name,used_memory --format=csv,noheader >> "$SAMPLES" 2>/dev/null || true
        sleep 1
    done
) &
SAMPLER=$!

set +e
nice -n 19 cargo test -j 6 --release --features cuda bench20 -- --ignored --nocapture
RC=$?
set -e

kill "$SAMPLER" 2>/dev/null || true
wait "$SAMPLER" 2>/dev/null || true
echo "== cuda run ep=cuda gpu=$GPU_NAME uuid=$UUID exit $RC"

# 8. Proof: bench binary on the 5070 Ti, and no bench process on another GPU.
SEEN="no"
PEAK=0
OTHER="no"
if [ -f "$SAMPLES" ]; then
    while IFS=, read -r uuid pid pname mem; do
        uuid="$(echo "$uuid" | xargs)"; pname="$(echo "$pname" | xargs)"; mem="$(echo "$mem" | xargs | sed 's/ MiB//')"
        case "$pname" in
            *wavelink_mixer*)
                if [ "$uuid" = "$UUID" ]; then
                    SEEN="yes"
                    if [ "${mem:-0}" -gt "$PEAK" ]; then PEAK="$mem"; fi
                else
                    OTHER="yes"
                fi
                ;;
        esac
    done < "$SAMPLES"
fi
echo "PROOF uuid=$UUID seen=$SEEN peak_mib=$PEAK"
echo "PROOF other_gpu_used=$OTHER"
echo "samples: $SAMPLES"
exit "$RC"
