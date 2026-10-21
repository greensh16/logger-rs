#!/bin/bash --login
#SBATCH --job-name=logger-rs-gpu-probe
#SBATCH --partition=gpu
#SBATCH --nodes=1
#SBATCH --ntasks=1
#SBATCH --gres=gpu:1
#SBATCH --time=00:05:00
#SBATCH --output=logger-rs-gpu-probe-%j.out
#
# Submit from the repository root, using a GPU-enabled account:
#   sbatch -A YOUR_ACCOUNT-gpu testing/probe_setonix_gpu.slurm
# Read-only probe. It does not run a GPU workload or change driver settings.

set -uo pipefail

if [[ "${1:-}" != "--inside-step" ]]; then
    ROOT="${SLURM_SUBMIT_DIR:-$PWD}"
    echo "job ${SLURM_JOB_ID:-?} on $(hostname)"
    echo "Launching one GPU-bound step for the actual device visibility and tool output."
    env -u SLURM_CPUS_PER_TASK -u SLURM_TRES_PER_TASK \
        srun --nodes=1 --ntasks=1 --cpus-per-task=8 --gres=gpu:1 \
        --export=ALL \
        bash --login "$ROOT/testing/probe_setonix_gpu.slurm" --inside-step
    exit $?
fi

echo "step host: $(hostname)"
echo "cgroup: $(cat /proc/self/cgroup 2>/dev/null)"
for key in SLURM_JOB_ID SLURM_JOB_GPUS SLURM_STEP_GPUS SLURM_GPUS_ON_NODE \
           ROCR_VISIBLE_DEVICES HIP_VISIBLE_DEVICES GPU_DEVICE_ORDINAL \
           CUDA_VISIBLE_DEVICES; do
    printf '%-26s %s\n' "$key" "${!key:-<unset>}"
done

echo "-- tools before loading ROCm --"
for tool in amd-smi rocm-smi rocminfo; do
    printf '%-10s %s\n' "$tool" "$(command -v "$tool" 2>/dev/null || echo absent)"
done

if type module >/dev/null 2>&1; then
    module load rocm craype-accel-amd-gfx90a 2>&1 || true
    module list 2>&1 || true
fi

echo "-- tools after loading ROCm --"
for tool in amd-smi rocm-smi rocminfo; do
    printf '%-10s %s\n' "$tool" "$(command -v "$tool" 2>/dev/null || echo absent)"
done

if command -v amd-smi >/dev/null 2>&1; then
    echo "-- amd-smi list --"
    timeout 15 amd-smi list 2>&1 | head -30
    echo "-- amd-smi metric --csv --"
    timeout 15 amd-smi metric --csv 2>&1 | head -30
    echo "-- amd-smi static --csv --"
    timeout 15 amd-smi static --csv 2>&1 | head -20
fi
if command -v rocm-smi >/dev/null 2>&1; then
    echo "-- rocm-smi CSV --"
    timeout 15 rocm-smi --showuse --showmemuse --showtemp --showpower --csv 2>&1 | head -30
fi

echo "Probe complete. Send the job output to implement AMD GPU sampling."
