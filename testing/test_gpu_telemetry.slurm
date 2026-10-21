#!/bin/bash --login
#SBATCH --job-name=logger-rs-gpu-test
#SBATCH --partition=gpu
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --gres=gpu:1
#SBATCH --time=00:05:00
#SBATCH --output=logger-rs-gpu-test-%j.out
#
# Submit from the repository root after rebuilding logger-rs with AMD support:
#   sbatch -A YOUR_ACCOUNT-gpu testing/test_gpu_telemetry.slurm

set -uo pipefail

ROOT="${SLURM_SUBMIT_DIR:-$PWD}"
if [[ "${1:-}" != "--inside-step" ]]; then
    env -u SLURM_CPUS_PER_TASK -u SLURM_TRES_PER_TASK \
        srun --nodes=1 --ntasks=1 --cpus-per-task=8 --gres=gpu:1 \
        --export=ALL bash --login "$ROOT/testing/test_gpu_telemetry.slurm" --inside-step
    exit $?
fi

BIN="$ROOT/logger-rs"
LIB="$ROOT/logger-rs.sh"
JOB_ID="${SLURM_JOB_ID:?must run inside a Slurm allocation}"
OUTDIR="$ROOT/testing/slurm-gpu-test-$JOB_ID"

fail() { echo "FAIL: $*" >&2; exit 1; }
[[ -x "$BIN" ]] || fail "binary not executable: $BIN"
[[ -r "$LIB" ]] || fail "wrapper not readable: $LIB"
command -v python3 >/dev/null 2>&1 || fail "python3 needed for verification"
mkdir -p "$OUTDIR" || fail "cannot create $OUTDIR"

if type module >/dev/null 2>&1; then
    module load rocm craype-accel-amd-gfx90a || fail "could not load ROCm"
fi
command -v amd-smi >/dev/null 2>&1 || fail "amd-smi is unavailable inside the GPU step"

echo "job: $JOB_ID"
echo "step host: $(hostname)"
echo "ROCR_VISIBLE_DEVICES: ${ROCR_VISIBLE_DEVICES:-<unset>}"
echo "output: $OUTDIR"
preflight="$("$BIN" --check --output "$OUTDIR/psutil_{jobid}_{host}.log" 2>&1)" \
    || fail "logger-rs --check failed: $preflight"
printf '%s\n' "$preflight"
[[ "$preflight" == *"amd-smi present; allocated AMD GPUs can be sampled"* ]] \
    || fail "binary predates AMD support: rebuild the current source with cargo build --release, then copy target/release/logger-rs to $BIN"

source "$LIB"
telemetry_start --telemetry-bin "$BIN" --gpu-interval 2 \
    --output "$OUTDIR/psutil_{jobid}_{host}.log" --interval 0.5
sleep 15
telemetry_stop 0 || true

python3 - "$OUTDIR" "$JOB_ID" <<'PY'
import glob
import json
import os
import sys

outdir, job_id = sys.argv[1:]
errors = []

def check(condition, message):
    print(("PASS" if condition else "FAIL") + ": " + message)
    if not condition:
        errors.append(message)

logs = glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log"))
check(len(logs) == 1, f"one node log: {len(logs)}")
for path in logs:
    with open(path, encoding="utf-8") as f:
        samples = [json.loads(line) for line in f if line.strip()]
    gpu_samples = [s for s in samples if s.get("gpu_indices")]
    check(bool(gpu_samples), "AMD GPU readings appeared in NDJSON")
    for sample in gpu_samples[:1]:
        check(sample.get("gpu_indices") == [0], "allocated GPU is device 0 inside the step")
        check(sample.get("gpu_is_node_scoped") is False, "GPU reading is job-scoped")
        total = sample.get("gpu_memory_total") or []
        check(len(total) == 1 and total[0] >= 60 * 1024**3,
              f"MI250X VRAM is at least 60 GiB: {total}")
        power = sample.get("gpu_power") or []
        check(len(power) == 1 and power[0] > 0,
              f"GPU power reading is positive: {power}")
        temperature = sample.get("gpu_temperature") or []
        check(len(temperature) == 1 and temperature[0] > 0,
              f"GPU temperature reading is positive: {temperature}")

summaries = glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log.summary.json"))
check(len(summaries) == 1, f"one node summary: {len(summaries)}")
for path in summaries:
    with open(path, encoding="utf-8") as f:
        summary = json.load(f)
    check(summary.get("gpu_indices") == [0], "summary retains assigned GPU index")
    check(summary.get("gpu_is_node_scoped") is False, "summary GPU scope is job")

print(f"Telemetry files: {outdir}")
sys.exit(1 if errors else 0)
PY
