#!/usr/bin/env python3
"""Check a completed Setonix walltime test from the repository root."""

import glob
import json
import os
import sys


def main() -> int:
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        print("usage: python3 testing/check_walltime.py JOB_ID", file=sys.stderr)
        return 2

    job_id = sys.argv[1]
    outdir = os.path.join(os.getcwd(), "testing", f"slurm-walltime-{job_id}")
    errors = []

    def check(condition: bool, message: str) -> None:
        print(("PASS" if condition else "FAIL") + ": " + message)
        if not condition:
            errors.append(message)

    marker = os.path.join(outdir, "checkpoint-check.txt")
    check(os.path.isfile(marker), "mid-run checkpoint observation exists")
    if os.path.isfile(marker):
        with open(marker, encoding="utf-8") as f:
            observation = f.read().strip()
        check(observation.startswith("PASS:"),
              "mid-run checkpoints: " + observation.partition(": ")[2])

    node_files = sorted(
        glob.glob(os.path.join(outdir, f"psutil_{job_id}_*.log.summary.json"))
    )
    check(len(node_files) == 2, f"two per-node summaries: {len(node_files)}")
    for path in node_files:
        with open(path, encoding="utf-8") as f:
            summary = json.load(f)
        host = summary.get("hostname", os.path.basename(path))
        check(summary.get("cpuacct_path", "").endswith(f"/job_{job_id}"),
              f"{host} measures the job cgroup")
        check(summary.get("samples", 0) >= 5, f"{host} kept sampling")
        check((summary.get("duration_sec") or 0) >= 90,
              f"{host} sampled close to the two-minute limit")
        print(f"INFO: {host} partial={summary.get('partial')} "
              f"duration={summary.get('duration_sec')}s "
              f"exit_status={summary.get('exit_status')}")

    merged_path = os.path.join(outdir, f"psutil_{job_id}.summary.json")
    check(os.path.isfile(merged_path), "job-level merge survived walltime termination")
    if os.path.isfile(merged_path):
        with open(merged_path, encoding="utf-8") as f:
            merged = json.load(f)
        check(merged.get("schema") == "merged-v1", "merged summary schema")
        check(merged.get("num_nodes_reporting") == 2, "both nodes reported")
        check(not merged.get("nodes_missing"), "no missing nodes")
        check((merged.get("duration_sec") or 0) >= 90,
              "merged duration reaches close to the time limit")
        print(f"INFO: nodes_partial={merged.get('nodes_partial')} "
              f"exit_status={merged.get('exit_status')} "
              f"duration={merged.get('duration_sec')}s")

    print(f"Telemetry files: {outdir}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
