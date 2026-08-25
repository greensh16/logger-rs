# On-Gadi test

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
