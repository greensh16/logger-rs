#!/bin/bash
#SBATCH --job-name=logger-rs-probe
#SBATCH --account=CHANGE_ME
#SBATCH --partition=work
#SBATCH --nodes=2
#SBATCH --ntasks-per-node=1
#SBATCH --cpus-per-task=8
#SBATCH --mem=16G
#SBATCH --time=00:10:00
#SBATCH --output=logger-rs-probe-%j.out
#
# ---------------------------------------------------------------------------
# A *probe*, not a test.
#
# Before logger-rs can be made to work on Setonix, three things need to be
# known rather than assumed, and none of them can be determined from outside
# the machine:
#
#   1. Where Slurm puts the job's cgroup, and whether it is v1 or v2.
#      Slurm's v2 layout nests a *step* cgroup under the *job* cgroup. Attaching
#      to the step would measure one `srun` invocation rather than the job, so
#      the exact shape decides which level the logger must walk up to.
#
#   2. Which cgroup controllers the site actually delegates. On Gadi `blkio`
#      is not delegated and reading it reported the PBS daemon's lifetime I/O
#      as the job's — hundreds of GB for a job that wrote under one. Whether
#      Pawsey delegates io/memory/cpu to the job is a site policy question with
#      the same class of consequence.
#
#   3. Which AMD GPU tool exists and what its output looks like. `amd-smi` and
#      `rocm-smi` have different CSV shapes, and the parser has to match the
#      one actually installed.
#
# This script answers those by printing what it finds. It changes nothing and
# measures nothing. Run it, send back the output, and the real implementation
# can be written against facts.
#
# Submit with:  sbatch testing/probe_setonix.slurm
# ---------------------------------------------------------------------------

set -uo pipefail   # deliberately no -e: a probe should survive its own failures

hr() { printf '\n=== %s %s\n' "$1" "$(printf '=%.0s' $(seq 1 $((60 - ${#1}))))"; }
kv() { printf '  %-34s %s\n' "$1" "${2:-<unset>}"; }

echo "logger-rs Setonix probe"
echo "date: $(date -u +%FT%TZ)   host: $(hostname)"

# ---------------------------------------------------------------------------
hr "1. Slurm environment"
# These are what the Rust side now reads for job identity and booked resources.
# Anything <unset> here is a gap the shell library has to cover via scontrol.
for v in SLURM_JOB_ID SLURM_JOBID SLURM_JOB_NAME SLURM_JOB_PARTITION \
         SLURM_JOB_ACCOUNT SLURM_JOB_NUM_NODES SLURM_NNODES \
         SLURM_NTASKS_PER_NODE SLURM_CPUS_PER_TASK SLURM_CPUS_ON_NODE \
         SLURM_JOB_CPUS_PER_NODE SLURM_MEM_PER_NODE SLURM_MEM_PER_CPU \
         SLURM_TIMELIMIT SLURM_JOB_NODELIST SLURM_SUBMIT_DIR \
         SLURM_JOB_GPUS SLURM_GPUS_ON_NODE GPU_DEVICE_ORDINAL \
         ROCR_VISIBLE_DEVICES HIP_VISIBLE_DEVICES CUDA_VISIBLE_DEVICES; do
  kv "$v" "${!v:-}"
done

echo
echo "  -- every SLURM_* variable, in case something useful is not on the list:"
env | grep -E '^SLURM' | sort | sed 's/^/     /'

# ---------------------------------------------------------------------------
hr "2. Booked walltime as scontrol reports it"
# SLURM_TIMELIMIT is frequently not exported. If so, this is the fallback the
# shell library will use, and its exact format matters to the parser.
if command -v scontrol >/dev/null 2>&1; then
  echo "  scontrol show job -o \$SLURM_JOB_ID | tr ' ' '\\n' | grep -i timelimit"
  scontrol show job -o "${SLURM_JOB_ID:-0}" 2>/dev/null \
    | tr ' ' '\n' | grep -iE 'timelimit|numnodes|numcpus|mem' | sed 's/^/     /'
else
  echo "  scontrol not on PATH"
fi

# ---------------------------------------------------------------------------
hr "3. cgroup layout  <-- the important one"
echo "  cgroup version indicators:"
kv "/sys/fs/cgroup/cgroup.controllers exists (v2)" \
   "$([ -e /sys/fs/cgroup/cgroup.controllers ] && echo yes || echo no)"
kv "/sys/fs/cgroup/cpuacct exists (v1)" \
   "$([ -d /sys/fs/cgroup/cpuacct ] && echo yes || echo no)"

echo
echo "  /proc/self/cgroup:"
cat /proc/self/cgroup 2>/dev/null | sed 's/^/     /'

echo
echo "  cgroup mounts from /proc/mounts:"
grep cgroup /proc/mounts 2>/dev/null | sed 's/^/     /'

echo
echo "  v2 delegated controllers at the root:"
cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null | sed 's/^/     /' || echo "     (none / not v2)"

# Walk from this process's cgroup up to the root, showing what exists at each
# level. This is what reveals whether the job cgroup sits above the step, and
# which level has populated counters.
echo
echo "  walking up from this process's cgroup:"
REL=$(awk -F: '$2=="" {print $3; exit}' /proc/self/cgroup 2>/dev/null)
[ -z "${REL:-}" ] && REL=$(awk -F: '/cpuacct|cpu,/ {print $3; exit}' /proc/self/cgroup 2>/dev/null)
BASE=/sys/fs/cgroup
CUR="$REL"
while [ -n "$CUR" ] && [ "$CUR" != "/" ]; do
  D="$BASE$CUR"
  if [ -d "$D" ]; then
    printf '     %s\n' "$CUR"
    printf '        procs=%s  controllers=[%s]\n' \
      "$(wc -l < "$D/cgroup.procs" 2>/dev/null || echo '?')" \
      "$(cat "$D/cgroup.controllers" 2>/dev/null | tr '\n' ' ')"
    for f in cpu.stat memory.current memory.max io.stat cpuacct.usage memory.usage_in_bytes; do
      [ -r "$D/$f" ] && printf '        %-24s %s\n' "$f" "$(head -c 120 "$D/$f" | tr '\n' ' ')"
    done
  fi
  CUR="${CUR%/*}"
done

# ---------------------------------------------------------------------------
hr "4. GPU tooling"
for tool in amd-smi rocm-smi nvidia-smi; do
  if command -v "$tool" >/dev/null 2>&1; then
    echo "  $tool: FOUND at $(command -v $tool)"
    "$tool" --version 2>&1 | head -3 | sed 's/^/     /'
  else
    echo "  $tool: not on PATH"
  fi
done

if command -v amd-smi >/dev/null 2>&1; then
  echo
  echo "  -- amd-smi metric output (this is what the parser must read):"
  amd-smi metric --csv 2>&1 | head -12 | sed 's/^/     /'
  echo "  -- amd-smi static (for device ids/uuids):"
  amd-smi static --csv 2>&1 | head -8 | sed 's/^/     /'
fi
if command -v rocm-smi >/dev/null 2>&1; then
  echo
  echo "  -- rocm-smi CSV output:"
  rocm-smi --showuse --showmemuse --showtemp --showpower --csv 2>&1 | head -12 | sed 's/^/     /'
fi

# ---------------------------------------------------------------------------
hr "5. logger-rs --check, if the module is available"
if command -v logger-rs >/dev/null 2>&1; then
  echo "  logger-rs at $(command -v logger-rs)"
  logger-rs --version 2>&1 | sed 's/^/     /'
  logger-rs --check 2>&1 | sed 's/^/     /'
else
  echo "  logger-rs not on PATH — build it for the target and add it, or skip."
  echo "  (This section is optional; sections 1-4 are the ones that matter.)"
fi

# ---------------------------------------------------------------------------
hr "6. Remote launch mechanism"
# The multi-node path depends on srun being usable from inside an allocation
# to start one extra process per node, alongside the real workload.
echo "  srun: $(command -v srun 2>/dev/null || echo 'not found')"
echo "  scontrol show hostnames:"
scontrol show hostnames "${SLURM_JOB_NODELIST:-}" 2>/dev/null | sed 's/^/     /'
echo
echo "  -- can srun --overlap start one task per node while we hold the allocation?"
srun --nodes="${SLURM_JOB_NUM_NODES:-1}" --ntasks="${SLURM_JOB_NUM_NODES:-1}" \
     --ntasks-per-node=1 --overlap \
     bash -c 'echo "     reached $(hostname) as $(whoami), cgroup: $(awk -F: "\$2==\"\" {print \$3; exit}" /proc/self/cgroup)"' \
     2>&1 | head -10

hr "done"
echo "Send this whole output back. Sections 3 and 4 decide the implementation."
