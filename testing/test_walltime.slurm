#!/bin/bash
#SBATCH --job-name=logger-rs-walltime
#SBATCH --partition=work
#SBATCH --nodes=2
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=2G
#SBATCH --time=00:02:00
#SBATCH --output=logger-rs-walltime-%j.out
#
# Submit from the repository root:
#   sbatch -A YOUR_ACCOUNT testing/test_walltime.slurm
# After the job reaches its time limit, run:
#   python3 testing/check_walltime.py JOB_ID
# This job deliberately reaches its time limit. It is expected to appear as
# TIMEOUT in Slurm accounting, even when the telemetry check passes.

set -uo pipefail

ROOT="${SLURM_SUBMIT_DIR:-$PWD}"
BIN="$ROOT/logger-rs"
LIB="$ROOT/logger-rs.sh"
JOB_ID="${SLURM_JOB_ID:?must run inside a Slurm allocation}"
OUTDIR="$ROOT/testing/slurm-walltime-$JOB_ID"

fail() { echo "FAIL: $*" >&2; exit 1; }
[[ -x "$BIN" ]] || fail "binary not executable: $BIN"
[[ -r "$LIB" ]] || fail "wrapper not readable: $LIB"
command -v python3 >/dev/null 2>&1 || fail "python3 needed for checkpoint observation"
mkdir -p "$OUTDIR" || fail "cannot create $OUTDIR"

echo "job: $JOB_ID"
echo "output: $OUTDIR"
echo "This job will run until its two-minute limit."
"$BIN" --check --output "$OUTDIR/psutil_{jobid}_{host}.log" \
    || fail "logger-rs --check failed"

source "$LIB"
telemetry_start --telemetry-bin "$BIN" --summary-every 5 \
    --output "$OUTDIR/psutil_{jobid}_{host}.log"

# Record whether both nodes wrote partial summaries *before* the time limit.
# The marker lives on shared storage and survives the scheduler's kill.
(
    sleep 30
    python3 - "$OUTDIR" "$JOB_ID" <<'PY'
import glob
import json
import os
import sys

outdir, job_id = sys.argv[1:]
files = sorted(glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log.summary.json")))
passed = len(files) == 2
details = []
for path in files:
    try:
        with open(path) as f:
            summary = json.load(f)
        valid = summary.get("partial") is True and summary.get("samples", 0) >= 5
        passed = passed and valid
        details.append(f"{summary.get('hostname')}: partial={summary.get('partial')} samples={summary.get('samples')}")
    except (OSError, ValueError) as exc:
        passed = False
        details.append(f"{path}: {exc}")
with open(os.path.join(outdir, "checkpoint-check.txt"), "w") as f:
    f.write(("PASS" if passed else "FAIL") + ": " + "; ".join(details) + "\n")
PY
) &

# Keep a real step on each node until Slurm ends the allocation. The wrapper's
# trap and each logger's checkpoint/termination path are the subjects of this test.
env -u SLURM_CPUS_PER_TASK -u SLURM_TRES_PER_TASK \
    srun --nodes=2 --ntasks=2 --ntasks-per-node=1 --cpus-per-task=1 \
    --overlap --export=ALL sleep 600

# Reaching here means the step ended before the job's time limit.
status=$?
telemetry_stop "$status" || true
echo "step ended before time limit with status $status; inspect $OUTDIR" >&2
exit 1
