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

# Read both power interfaces in this same allocation, after telemetry stops.
# Their raw output is retained so a node-specific missing reading can be
# distinguished from a parser problem without booking another GPU job.
timeout 20 amd-smi metric --csv > "$OUTDIR/amd-smi-metric.csv" \
    2> "$OUTDIR/amd-smi-metric.stderr"
amd_status=$?
timeout 20 rocm-smi --showpower --csv > "$OUTDIR/rocm-smi-power.csv" \
    2> "$OUTDIR/rocm-smi-power.stderr"
rocm_status=$?
python3 - "$OUTDIR" "$amd_status" "$rocm_status" <<'PY'
import csv
import os
import sys

outdir, amd_status, rocm_status = sys.argv[1:]
print(f"Power diagnostics: amd-smi exit={amd_status}, rocm-smi exit={rocm_status}")
for filename, fields in (
    ("amd-smi-metric.csv", ("gpu", "socket_power")),
    ("rocm-smi-power.csv", ("device", "Average Graphics Package Power (W)")),
):
    with open(os.path.join(outdir, filename), encoding="utf-8") as f:
        lines = f.readlines()
    start = next((i for i, line in enumerate(lines)
                  if line.startswith(fields[0] + ",")), len(lines))
    rows = list(csv.DictReader(lines[start:]))
    print(filename, [{field: row.get(field) for field in fields} for row in rows[:4]])
PY

python3 "$ROOT/testing/check_gpu_telemetry.py" "$JOB_ID"
