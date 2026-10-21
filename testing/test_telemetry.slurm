#!/bin/bash
#SBATCH --job-name=logger-rs-test
#SBATCH --partition=work
#SBATCH --nodes=2
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=16G
#SBATCH --time=00:05:00
#SBATCH --output=logger-rs-test-%j.out
#
# Submit from the repository root:
#   sbatch -A YOUR_ACCOUNT testing/test_telemetry.slurm
# The binary and logger-rs.sh must be in that root directory and up to date.

set -uo pipefail

ROOT="${SLURM_SUBMIT_DIR:-$PWD}"
BIN="$ROOT/logger-rs"
LIB="$ROOT/logger-rs.sh"
JOB_ID="${SLURM_JOB_ID:?must run inside a Slurm allocation}"
OUTDIR="$ROOT/testing/slurm-test-$JOB_ID"

fail() { echo "FAIL: $*" >&2; exit 1; }
[[ -x "$BIN" ]] || fail "binary not executable: $BIN"
[[ -r "$LIB" ]] || fail "wrapper not readable: $LIB"
command -v python3 >/dev/null 2>&1 || fail "python3 is needed to verify the summaries"
mkdir -p "$OUTDIR" || fail "cannot create $OUTDIR"

echo "binary: $BIN"
echo "wrapper: $LIB"
echo "output: $OUTDIR"
"$BIN" --version || fail "binary cannot run on this node"
"$BIN" --check || fail "logger-rs --check failed on the batch node"

# The probe showed SLURM_CPUS_PER_TASK=9 and SLURM_TRES_PER_TASK=cpu=8.
# Only clear them for each new srun step; preserve the batch environment.
source "$LIB"
telemetry_start --telemetry-bin "$BIN" \
    --output "$OUTDIR/psutil_{jobid}_{host}.log" --interval 0.5

sleep 2
env -u SLURM_CPUS_PER_TASK -u SLURM_TRES_PER_TASK \
    srun --nodes=2 --ntasks=2 --ntasks-per-node=1 --cpus-per-task=1 \
    --overlap --export=ALL \
    bash -c 'echo "busy on $(hostname)"; start=$SECONDS; while ((SECONDS - start < 20)); do :; done'
workload_status=$?
sleep 2
telemetry_stop "$workload_status" || true

python3 - "$OUTDIR" "$JOB_ID" "$workload_status" <<'PY'
import glob
import json
import os
import sys

outdir, job_id, workload_status = sys.argv[1], sys.argv[2], int(sys.argv[3])
errors = []
expected = f"/job_{job_id}"

def check(condition, message):
    print(("PASS" if condition else "FAIL") + ": " + message)
    if not condition:
        errors.append(message)

check(workload_status == 0, f"workload exit status = {workload_status}")
merged_path = os.path.join(outdir, f"psutil_{job_id}.summary.json")
check(os.path.isfile(merged_path), "merged summary exists")
if os.path.isfile(merged_path):
    with open(merged_path) as f:
        merged = json.load(f)
    check(merged.get("num_nodes_reporting") == 2, "both nodes reported")
    check(not merged.get("nodes_missing"), "no missing nodes")
    check(merged.get("booked_walltime_sec") == 300, "five-minute booking recorded")
    check(25 <= merged.get("cpu_core_seconds", 0) <= 60,
          f"job CPU time near 40 core-seconds: {merged.get('cpu_core_seconds')}")

files = sorted(glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log.summary.json")))
check(len(files) == 2, f"two per-node summaries: {len(files)}")
for path in files:
    with open(path) as f:
        node = json.load(f)
    host = node.get("hostname", os.path.basename(path))
    cgroup = node.get("cpuacct_path", "")
    check(cgroup.endswith(expected), f"{host} measures job cgroup: {cgroup}")
    check(node.get("proc_source") == "cgroup.procs",
          f"{host} reads job cgroup membership")
    check(node.get("cpu_core_seconds", 0) >= 10,
          f"{host} measured the busy step")

print(f"Telemetry files: {outdir}")
sys.exit(1 if errors else 0)
PY
