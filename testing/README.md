# On-cluster tests

## Setonix (Slurm)

From the repository root on Setonix, with a current `logger-rs` binary and
`logger-rs.sh` in that directory:

```bash
sbatch -A YOUR_ACCOUNT testing/test_telemetry.slurm
```

The test runs one CPU-bound task per node for 20 seconds, then a separate
`srun` step that touches and holds 256 MiB per node. It checks the two node
summaries and their merge, job-level cgroup selection, CPU time, and kernel
memory peaks. The extended test passed as job 51292897 (2026-10-21): 39.40
core-seconds merged, both memory peaks above 256 MiB, and no missing nodes.
Its output is under `testing/slurm-test-<jobid>/`.

To test checkpoints and shutdown at the scheduler's time limit, submit a
separate two-minute job, then run the checker after it ends:

```bash
sbatch -A YOUR_ACCOUNT testing/test_walltime.slurm
python3 testing/check_walltime.py JOB_ID
```

The walltime job deliberately times out. It records whether both node
checkpoint files existed during the run, and the after-job checker verifies
the per-node summaries and merged summary. Job 51292900 passed: both nodes
checkpointed during the run and the merged summary survived the time limit.
One node finished cleanly with exit status 143; the other remained partial, so
its counters are lower bounds through its last checkpoint.

With access to a GPU account, capture GPU visibility and AMD tooling from an
actual GPU step:

```bash
sbatch -A YOUR_ACCOUNT-gpu testing/probe_setonix_gpu.slurm
```

The probe prints Slurm's GPU assignment, the step's cgroup, visible-device
variables, and output from available `amd-smi` and `rocm-smi` commands. It
does not run a GPU workload. Job 51292904 confirmed that the allocated MI250X
appears as device 0 inside the step (`ROCR_VISIBLE_DEVICES=0`), although
`SLURM_JOB_GPUS=6` names its physical Slurm index. `amd-smi metric --csv`
reported GPU use, VRAM, power and temperature.

After rebuilding the binary with AMD GPU sampling and copying it to the
repository-root `logger-rs` path, submit the telemetry acceptance test. It
checks the preflight message for AMD support before sampling the allocated GPU
while idle, then checks the NDJSON and summary fields:

```bash
sbatch -A YOUR_ACCOUNT-gpu testing/test_gpu_telemetry.slurm
```

Job 51293735 ran the acceptance script with an older binary: its preflight
said "no nvidia-smi" and it produced no GPU readings. The updated script now
stops immediately with a rebuild instruction when it detects that binary.
Job 51293839 passed the GPU metric and scope checks using the rebuilt binary:
65,520 MiB total VRAM, 140 W and 37 C were recorded for assigned device 0.
It wrote eight samples during the 15-second test, indicating that synchronous
AMD SMI queries may delay the 0.5-second CPU sampling cadence. GPU polling
now runs in a background worker. Job 51294237 recorded 31 samples in its
15-second window, confirming the CPU cadence recovered. Its first GPU reading
and every subsequent reading had power 0 W, so power collection remains
unverified on that node. The sampler now tries `rocm-smi --showpower --csv`
when AMD SMI power is missing. The next acceptance run saves both tools' raw
power output in its telemetry directory and prints their power columns.
The checker inspects all readings and can be rerun on existing output:

```bash
python3 testing/check_gpu_telemetry.py 51294237
```

GPU utilization has not been validated against a known load.

Setonix's `work` partition exposed job-level CPU and memory accounting under
known loads. The GPU probe exercised AMD SMI but not the logger. No job-level
`io.stat` was exposed; cgroup v2 has no per-core CPU breakdown. Network
counters remain node-wide.

## Gadi (PBS)

`test_telemetry.pbs` is an end-to-end test of the logger on real hardware. It
runs a workload whose resource usage is known in advance, then **checks** the
telemetry against it rather than just producing some, and exits non-zero if
anything is wrong.

Nearly all of this crate's test coverage is unit tests that never touch a real
cgroup. This is the thing that exercises the parts that cannot be tested
anywhere else.

## Before submitting

1. Build and copy the binary and wrapper to Gadi:

   ```bash
   ./build-linux.sh
   scp target/x86_64-unknown-linux-musl/release/logger-rs gadi.nci.org.au:~/bin/
   scp logger-rs.sh gadi.nci.org.au:~/bin/
   ```

2. Submit:

   ```bash
   qsub testing/test_telemetry.pbs
   ```

Takes about 5 minutes of the 20 requested. Output goes to the job's `.o` file.

## Where things land

Two directories, deliberately separate:

| | Path | Contents |
|---|---|---|
| **Telemetry** | `/g/data/gb02/sg7549/dashboard_logs` | The NDJSON, per-node summaries, merged summary and manifest — what the dashboard reads |
| **Scratch** | `$PBS_O_WORKDIR/telemetry-test-<jobid>/` | `workload.sh`, `driver.sh`, `check.py` |

The telemetry directory is **shared across every run**, which is what the
dashboard's job list wants — point a profile's `log_dirs` at it and every job
shows up. It is not a per-run scratch area, so the test's own helper scripts
stay out of it, and every check in the report is scoped to this job's id rather
than to "any `psutil_*` file in this directory".

That scoping matters: with four jobs' files present, an unscoped
`*.log.summary.json` matches nine files, and a perfectly good two-node run
would fail with "found 9 per-node summaries, expected 2".

Both paths are overridable:

```bash
qsub -v OUTDIR=/g/data/gb02/sg7549/scratch_logs testing/test_telemetry.pbs
```

The `#PBS -l storage=gdata/gb02` directive is what makes `/g/data/gb02` visible
inside the job. Without it the directory simply is not mounted, and the run
stops at preflight with an explicit message rather than writing somewhere
useless.

## What it covers

The workload runs four phases on **every** node — fanned out with `pbs_tmrsh`,
so the remote loggers have something real to measure. Without that you could not
tell a broken logger from an idle node.

| Phase | What it does | What it proves |
|---|---|---|
| CPU | `BUSY_CORES` cores busy for `BUSY_SECS` | core-seconds are right, on every node |
| Memory | allocate and touch `MEM_MB` | cgroup memory sees it; shows the summed-RSS overcount |
| Fork | spawn and reap 200 short-lived children doing I/O | I/O totals survive child exit |
| Idle | a quiet stretch | utilisation is not flat |

The checks that matter most:

- **The cgroup path names the job.** If it looks like the root cgroup, the run
  fails loudly — that means the logger measured the whole node, which is exactly
  the bug that `--tree-pid 1` on remote nodes used to cause, and it produces
  plausible-looking numbers while being completely wrong.
- **`proc_source` is `cgroup.procs`** on remote nodes, not a process-tree walk.
- **Every allocated node reported**, and `nodes_missing` is empty.
- **`cpu_core_seconds` is within tolerance** of `BUSY_CORES x BUSY_SECS x nodes`.
- **Each NDJSON file contains samples from exactly one host** — if two nodes
  wrote to the same file, the `{host}` templating failed.
- Peak bounds are ordered (`max_node <= sum_of_nodes`), exit status propagates,
  and the merged summary carries `"schema": "merged-v1"`.

## Knobs

All overridable from the environment, so you can shrink the run:

```bash
qsub -v BUSY_CORES=4,BUSY_SECS=20,MEM_MB=512 testing/test_telemetry.pbs
```

| Variable | Default | |
|---|---|---|
| `BUSY_CORES` | 24 | clamped to cores per node |
| `BUSY_SECS` | 60 | |
| `MEM_MB` | 4096 | per node |
| `FORK_CHILDREN` | 200 | |
| `TELEMETRY_BIN` | `~/bin/logger-rs` | |
| `TELEMETRY_LIB` | `~/bin/logger-rs.sh` | |

**Single node:** change `#PBS -l ncpus=96` to `48`. The script detects the node
count and adjusts its expectations.

## Reading the result

A `WARN` is not necessarily a failure. The two you should expect:

- `fell back to a process-tree walk` on the **head node** is normal — it has a
  real process tree to walk, and the wrapper lets it use one.
- `no cgroup I/O figure` means the `blkio` (v1) or `io` (v2) controller is not
  delegated to jobs on that host. Worth knowing, but not a logger bug.

A `FAIL` on the cgroup path check, on node count, or on core-seconds is real and
worth chasing before trusting any dashboard numbers.

## Verifying the checker itself

The report logic was exercised locally against synthetic inputs built from the
real struct definitions, including the negative cases: a root-cgroup path and a
node that never reported both correctly fail. What that harness could *not*
cover — and what this job is for — is whether the cgroup is discovered correctly
on a real Gadi node, whether `cgroup.procs` lists what we expect, and whether
`pbs_tmrsh` places the remote loggers where we think.
