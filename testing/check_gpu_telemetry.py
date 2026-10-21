#!/usr/bin/env python3
"""Check a Setonix GPU telemetry run from the repository root."""

import glob
import json
import os
import sys


def main() -> int:
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        print("usage: python3 testing/check_gpu_telemetry.py JOB_ID", file=sys.stderr)
        return 2

    job_id = sys.argv[1]
    testing_dir = os.path.dirname(os.path.abspath(__file__))
    outdir = os.path.join(testing_dir, f"slurm-gpu-test-{job_id}")
    errors = []

    def check(condition: bool, message: str) -> None:
        print(("PASS" if condition else "FAIL") + ": " + message)
        if not condition:
            errors.append(message)

    logs = glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log"))
    check(len(logs) == 1, f"one node log: {len(logs)}")
    for path in logs:
        with open(path, encoding="utf-8") as f:
            rows = [json.loads(line) for line in f if line.strip()]
        samples = [row for row in rows if row.get("event") == "job_log"]
        check(len(samples) >= 20,
              f"CPU sampling stayed near its 0.5-second cadence: {len(samples)} samples")

        gpu_samples = [sample for sample in samples if sample.get("gpu_indices")]
        check(bool(gpu_samples), "AMD GPU readings appeared in NDJSON")
        if gpu_samples:
            check(all(s.get("gpu_indices") == [0] for s in gpu_samples),
                  "allocated GPU is device 0 inside the step")
            check(all(s.get("gpu_is_node_scoped") is False for s in gpu_samples),
                  "GPU readings are job-scoped")
            totals = [s["gpu_memory_total"][0] for s in gpu_samples
                      if len(s.get("gpu_memory_total") or []) == 1]
            check(bool(totals) and min(totals) >= 60 * 1024**3,
                  f"MI250X VRAM is at least 60 GiB: {totals[:3]}")
            powers = [s["gpu_power"][0] for s in gpu_samples
                      if len(s.get("gpu_power") or []) == 1]
            check(any(power > 0 for power in powers),
                  f"GPU power had a positive reading: {powers}")
            temperatures = [s["gpu_temperature"][0] for s in gpu_samples
                            if len(s.get("gpu_temperature") or []) == 1]
            check(any(temp > 0 for temp in temperatures),
                  f"GPU temperature had a positive reading: {temperatures}")

    summaries = glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log.summary.json"))
    check(len(summaries) == 1, f"one node summary: {len(summaries)}")
    for path in summaries:
        with open(path, encoding="utf-8") as f:
            summary = json.load(f)
        check(summary.get("gpu_indices") == [0], "summary retains assigned GPU index")
        check(summary.get("gpu_is_node_scoped") is False, "summary GPU scope is job")

    print(f"Telemetry files: {outdir}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
