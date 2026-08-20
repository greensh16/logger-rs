# HPC Telemetry Logger

High-performance telemetry collection for HPC jobs, written in Rust.

**`hpc-telemetry`** samples a job's resource usage from its cgroup and writes
NDJSON to a file on gdata. The dashboard watcher on Nirin reads those files
directly — there is no HTTP streaming component.

Supports both **cgroup v1** (Gadi today) and **cgroup v2** (unified hierarchy,
what a RHEL 9 refresh would bring); the hierarchy in use is detected at startup
and recorded in the output.

## Quick start

Put two lines around the part of your job script you want measured:

```bash
module use /g/data/gb02/modules
module load hpc-telemetry
source hpc-telemetry.sh

./stage-inputs.sh                            # setup — not measured

telemetry_start
    mpirun -np "$PBS_NCPUS" ./my_model       # measured
    ./postprocess                            # measured
telemetry_stop

./copy-results-back.sh                       # clean-up — not measured
```

Your script keeps its shape. Anything can go between `telemetry_start` and
`telemetry_stop` — pipelines, redirections, several launches, a command line
assembled in variables — none of which survive being prefixed with a launcher,
which is why that approach was dropped.

Marking a region rather than the whole script also keeps **data staging out of
the numbers**. A twenty-minute copy from `/g/data` to jobfs, measured, looks
like your job running at nearly zero CPU efficiency: it drags every figure down
and makes the dashboard recommend fewer cores than the science actually needs.

The output path defaults to `$HPC_DASHBOARD_DIR` (or the working directory), so
`--output` is only needed when you want somewhere specific.

A complete, commented job script is in
[`examples/gadi-job.pbs`](examples/gadi-job.pbs) — copy it and change four lines.

### How `source hpc-telemetry.sh` finds the file

`source` with a bare filename searches `$PATH` (bash's `sourcepath` option, on
by default), and `PATH` takes precedence over the working directory, so a
stray local file of the same name cannot shadow it. The modulefile puts the
install directory on `PATH`, so nothing else is needed.

`hpc-telemetry.sh` therefore has to sit **in the same `bin/` directory as the
`hpc-telemetry` binary** — the one the modulefile prepends. Not in
`/g/data/gb02/modules`: that is `MODULEPATH`, which holds modulefiles, and
`module use` never adds it to `PATH`. See
[`examples/modulefile`](examples/modulefile).

If your site disables `sourcepath`, the bare form fails, `telemetry_start` is
never defined, and calling it returns 127 — which under `set -e` would abort
the job over its own instrumentation. The modulefile also exports
`HPC_TELEMETRY_SH`, so a script that wants to be certain can write:

```bash
source "${HPC_TELEMETRY_SH:-hpc-telemetry.sh}"
```

### telemetry_stop is optional

If the script ends, exits early under `set -e`, or PBS terminates it at the
walltime limit, an `EXIT` trap performs the same shutdown. Call it explicitly
where you can: it records your workload's exit status directly, and it keeps
clean-up out of the measurement.

The one case neither covers is `SIGKILL`, which no trap can catch — a job hard
-killed records an unknown outcome. Ordinary walltime expiry and `qdel` send
`SIGTERM` first and are handled.

### Nothing here can fail your job

A missing binary, an unwritable output directory, a logger that dies on
startup: each is reported and then ignored, and `telemetry_start` always
returns success. Under `set -e` — which most production scripts use — anything
else would abort the job over its instrumentation. Running unmeasured is far
better than not running.

### Check it will work before you queue

```bash
hpc-telemetry --check
```

Run inside a short interactive job. It reports the cgroup version and scope,
which controllers the job actually owns, whether the output path is writable,
and what will therefore be missing — in seconds, rather than after a queue wait.
Everything it looks for otherwise produces a plausible-looking log file that is
quietly wrong, which is harder to notice than a crash.

### What this does that a backgrounded binary cannot

- The logger runs as a *sibling* of your workload and cannot see its exit
  status. Started by hand, every job is recorded with an **unknown** outcome.
- PBS starts your job script on one node only, so on a multi-node job every
  other node would go unmeasured. `telemetry_start` launches a logger on
  **every node in the allocation** and `telemetry_stop` merges their summaries
  into one job-level file.

## Multi-node jobs

Each node writes its own NDJSON and its own summary; nothing is sent between
nodes while the job runs. When the workload finishes, the wrapper runs
`hpc-telemetry --merge` to produce one job-level summary alongside them.

```
/g/data/ab12/dashboard/sam/
  psutil_12345.gadi-pbs.manifest.json                 <- written first, lists everything
  psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log        <- per node
  psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log.summary.json
  psutil_12345.gadi-pbs_gadi-cpu-clx-0124.log
  psutil_12345.gadi-pbs_gadi-cpu-clx-0124.log.summary.json
  psutil_12345.gadi-pbs.summary.json                  <- merged, "schema": "merged-v1"
```

The merged file is identified by `"event": "job_summary"` and
`"schema": "merged-v1"`, so the watcher can tell it from a per-node summary
without parsing filenames. Every per-node record also carries `hostname`.

## Reading a job from the dashboard

**Start from the manifest.** Its name is derivable from the job id alone —
`psutil_<jobid>.manifest.json` — so the poller never has to glob a directory or
parse hostnames out of filenames. It is written before any logger starts, for
single-node jobs as well as multi-node, so there is no special case.

```json
{
  "event": "job_manifest",
  "schema": "manifest-v1",
  "job_id": "12345.gadi-pbs",
  "nodes_expected": 2,
  "files": [
    {"hostname": "gadi-cpu-clx-0123", "node_rank": 0,
     "log": "psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log",
     "summary": "psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log.summary.json"},
    {"hostname": "gadi-cpu-clx-0124", "node_rank": 1, "...": "..."}
  ],
  "merged_summary": "psutil_12345.gadi-pbs.summary.json"
}
```

Filenames are relative to the directory the manifest was found in, so the job
does not need to know where gdata is mounted on the reader's side.

`nodes_expected` is what makes partial state legible: a listed file that does
not exist yet is a node still starting, and one that stops growing while others
continue is a node whose logger died. Without it neither is distinguishable from
a node that was never part of the job.

### Why not one combined file

The NDJSON is a **live** stream. A combined file could only be written after the
job ends — which is precisely when live tracking stops being useful — so
multi-node jobs would show nothing at all until they finished. Having every node
append to one shared file instead is worse: concurrent `O_APPEND` across nodes
serialises on Lustre lock contention, and at ~1.1 KB per line there is no
atomicity guarantee, so lines can interleave and tear.

Combining also would not save the reader the actual work. Samples from different
nodes arrive at slightly different timestamps and each node's `cpu_pct_sum` is
its own, so producing a job-level series means bucketing by time and summing
across hosts either way. One file would save a `glob()`, not the aggregation.

### Aggregating to a job-level series

```python
# One read offset per file; re-poll each every 15-30s as now.
async def poll_job(dirpath, job_id, offsets):
    manifest = json.loads((dirpath / f"psutil_{job_id}.manifest.json").read_text())

    for entry in manifest["files"]:
        path = dirpath / entry["log"]
        if not path.exists():
            continue                      # node still starting
        with path.open() as f:
            f.seek(offsets.get(entry["log"], 0))
            for line in f:
                if line.endswith("\n"):   # ignore a partially-flushed tail
                    yield json.loads(line)
            offsets[entry["log"]] = f.tell()

# Bucket to the sampling interval, then sum across nodes.
def to_job_series(samples, bucket_sec=1.0):
    buckets = defaultdict(dict)           # t -> {hostname: cpu_pct_sum}
    for s in samples:
        if s.get("event") != "job_log":
            continue
        t = round(s["t"] / bucket_sec) * bucket_sec
        buckets[t][s["hostname"]] = s["cpu_pct_sum"]

    # Sum per bucket. Keep the node count so a bucket missing a node is visible
    # rather than silently reading as a dip in utilisation.
    return [
        {"t": t,
         "cpu_pct_sum": sum(by_host.values()),
         "nodes_reporting": len(by_host),
         "nodes_expected": len(manifest["files"])}
        for t, by_host in sorted(buckets.items())
    ]
```

Two things worth getting right:

- **Do not treat a missing node in a bucket as zero.** Loggers start a second or
  two apart and tick independently, so early and late buckets legitimately have
  fewer nodes. Carrying `nodes_reporting` alongside the sum lets the frontend
  show a partial bucket as partial instead of as a utilisation dip.
- **Only parse complete lines.** The logger flushes at least every 2s, so a poll
  can catch a partly-written final line. Checking for the trailing newline and
  leaving the offset before it is enough.

**How it works.** The output path may contain `{host}`, `{jobid}` and `{user}`,
expanded on the node that does the writing — so one template is broadcast to
every node and each fills in its own name. Loggers are launched with `pbsdsh`
(PBS) or `srun` (Slurm). They shut down by watching for the exit-status file the
head node writes to gdata, because neither launcher reliably forwards SIGTERM to
the task it started. If no launcher is available the wrapper says so and
measures the head node only.

**What the merge can and cannot combine.** Totals — core-seconds, I/O bytes,
network bytes, page faults — are exact sums, additive no matter when each node
was busy. Peaks are not: two nodes each peaking at 90% may have done so seconds
apart, so the job never used 180% at once. Rather than pick a number and pretend,
the merged summary reports both bounds:

| field | meaning |
|---|---|
| `cpu_pct_sum_peak_max_node` | a value some node definitely reached |
| `cpu_pct_sum_peak_sum_of_nodes` | an upper bound the job cannot have exceeded |

The true simultaneous peak lies between them; recovering it exactly would need
the per-node sample streams aligned on a common clock. `cpu_efficiency_pct_avg`
*is* exact — it is a ratio of two sums, not a peak.

**Missing nodes.** `nodes_missing` lists allocated nodes that produced no
summary, and `num_nodes_reporting` says how many did. If a logger was killed,
its node's usage is absent from the totals — the merged file says so rather than
silently reporting a fraction of the job as the whole.

To measure only the head node, pass `--single-node`.

## Metrics collected

### Per sample (`job_log`)

| Metric | Source | Scope |
|---|---|---|
| CPU total and per-core % | cgroup cpu accounting | job |
| CPU efficiency vs booked cores | derived | job |
| Memory (current, kernel peak) | cgroup memory controller | job |
| Swap | cgroup memory controller | job |
| Block I/O bytes and ops | cgroup io/blkio controller | job |
| Process, thread, FD counts | `/proc` tree walk | job |
| Page faults (major/minor) | `/proc` tree walk | job |
| Summed RSS *(legacy)* | `/proc` tree walk | job |
| Network bytes/packets | `/proc/net/dev` | **node** |
| GPU util/memory/temp/power | `nvidia-smi` | job, or **node** — see below |
| Scheduler topology | PBS/Slurm environment | job |

### Summary (written on exit)

Peaks for every metric, CPU core-seconds and core-hours, walltime and memory
efficiency against the booking, per-GPU peaks, network totals, and the job's
exit status and reason.

> **Metric scope.** Every sample carries `net_is_node_scoped` and
> `gpu_is_node_scoped` so the dashboard never has to guess.
>
> **GPU** readings are filtered to the job's own devices when
> `CUDA_VISIBLE_DEVICES` is set — which PBS and Slurm both do for GPU jobs —
> using either the index or the (possibly abbreviated) UUID form.
> `gpu_is_node_scoped` is then `false`, and `gpu_indices` carries the node index
> of each reading so "GPU 1 was busy" stays distinguishable from "one of the
> node's GPUs was busy". Without that variable set we cannot tell which GPUs are
> ours, so the readings stay node-wide and are labelled as such. A job with
> `CUDA_VISIBLE_DEVICES=""` never forks `nvidia-smi` at all.
>
> **Network** figures are always node-wide. `/proc/net/dev` has no notion of job
> ownership, and there is no cheap per-job alternative without the `net_cls`
> cgroup controller or eBPF, neither of which is available to an unprivileged
> job on Gadi. Treat them as an upper bound: exact on an exclusive node,
> inflated on a shared queue.

### Controllers that are not delegated to the job

A cgroup controller is only used if its path belongs to *this job*. Sites do not
delegate every controller, and reading one that belongs to something else gives
confidently wrong numbers.

**Gadi is a live example.** A job's own `/proc/self/cgroup` reads:

```
9:cpu,cpuacct:/pbspro.service/jobid/12345.gadi-pbs   <- the job
2:memory:     /pbspro.service/jobid/12345.gadi-pbs   <- the job
4:cpuset:     /pbspro.service/jobid/12345.gadi-pbs   <- the job
5:blkio:      /system.slice/pbs.service              <- the PBS daemon
```

`blkio` is not delegated. Reading it reported the PBS daemon's lifetime I/O
across every job on the node — hundreds of GB for a job that wrote under a GB,
and differing by 147 GB between two nodes running identical work. The logger now
notes the mismatch on stderr, reports `cgroup_io_*` as absent, and falls back to
the per-process counters.

So **on Gadi today, I/O comes from `/proc/{pid}/io`**. CPU, memory and cpuset are
all properly job-scoped.

The per-process fallback is best-effort: because the counters are summed over
*live* processes, a job that forks and reaps children faster than the sampling
interval can lose I/O between samples. On the validation job (200 children each
writing 4 MB, reaped in batches of 20) it recorded 801 MB and 800 MB on the two
nodes — matching the workload — but that is not a guarantee, and finer-grained
child churn will under-report. Treat it as a lower bound.

## Building

```bash
cargo build --release
cargo test
```

### Cross-compile for Linux (from macOS)

```bash
brew install filosottile/musl-cross/musl-cross
rustup target add x86_64-unknown-linux-musl
./build-linux.sh
```

Produces a ~1.1 MB fully static binary that runs on any Linux x86_64 host.

The binary builds and runs on macOS for development, but with no cgroups
available it runs in stub mode and collects nothing.

## Usage

The wrapper covers the common case. To run the binary directly:

```bash
hpc-telemetry \
  --output /g/data/ab12/dashboard/$USER/psutil_$PBS_JOBID.log \
  --interval 0.5
```

| Argument | Default | Notes |
|---|---|---|
| `--output`, `--outfile` | *required* | NDJSON output path. Supports `{host}`, `{jobid}`, `{user}`. Parent directories are created. |
| `--merge DIR` | — | Merge the per-node summaries in DIR into one job-level summary and exit. Needs `--job-id`. |
| `--merge-expect-nodes` | — | Comma-separated allocation, so the merge can report nodes that never reported. |
| `--node-rank` | — | This node's index in the allocation. |
| `--summary` | `<output>.summary.json` | Summary JSON path. |
| `--interval` | `0.5` | Sampling interval, seconds. Must be ≥ 0.1. |
| `--gpu-interval` | `5.0` | GPU polling interval. Each poll forks `nvidia-smi`. |
| `--tree-pid` | parent PID | Root of the process tree to measure. |
| `--exit-status-file` | none | File the wrapper writes the workload's exit status to. |
| `--user-id` | `$USER` | |
| `--job-id` | `$PBS_JOBID` | |
| `--queue`, `--job-name`, `--project` | from PBS env | |
| `--booked-walltime`, `--booked-mem` | from PBS env | Used for efficiency calculations. |

Environment variables are read for all of the above; `--help` lists the exact
names.

## Output format

NDJSON — one JSON object per line. Three event types:

- **`job_start`** — once at the beginning: identity, booked resources, cgroup
  version, logger version.
- **`job_log`** — every `--interval` seconds: the metrics table above.
- **`job_end`** — once at shutdown: exit status, exit reason, duration, CPU
  core-seconds and core-hours.

`test_sample.ndjson` is a representative file, and is checked against the real
types by `tests/schema_contract.rs`.

### Schema stability

The field names are a contract with the dashboard, enforced by
`tests/schema_contract.rs`. New fields are added alongside old ones; every field
added since the first release is `#[serde(default)]`, so archived logs still
parse.

Some fields are retained only for backwards compatibility and should not be used
for new work:

| Legacy field | Use instead | Why |
|---|---|---|
| `rss_bytes_sum` | `cgroup_mem_bytes` | Summed RSS double-counts shared pages — badly wrong for MPI. |
| `io_*` | `cgroup_io_*` | Per-process I/O counters vanish when children exit. |
| `swap_bytes` | `cgroup_swap_bytes` | Same. |
| `system_cpu_efficiency` | `cpu_efficiency_pct` | The old field is a duplicate of `cpu_pct_sum`, not an efficiency. |
| `tree_percpu_pct` | `system_percpu_pct` | Identical; cgroup cpuacct cannot attribute per-CPU time to individual processes. |

### Service Units

The logger emits `cpu_core_hours`. Service Units are core-hours multiplied by
the queue's charge rate, which lives in the dashboard's rate table — the logger
has no way to know the rate for the queue it is running in.

## Architecture

```
src/
├── main.rs        # Entry point, signal handling, merge dispatch
├── lib.rs         # Library root (so integration tests share the code)
├── cli.rs         # Argument parsing, PBS env resolution, validation
├── host.rs        # Node identity
├── merge.rs       # Combining per-node summaries into a job total
├── types.rs       # Wire types: TelemetrySample, TelemetrySummary
├── cgroup.rs      # cgroup v1/v2 discovery and metric reads
├── cpu.rs         # CPU percentage and efficiency calculations
├── process.rs     # Process tree traversal
├── output.rs      # NDJSON writer, atomic summary write
├── logger.rs      # Main sampling loop
├── gpu.rs         # GPU metrics (rate-limited nvidia-smi)
├── network.rs     # Network statistics
└── scheduler.rs   # PBS/Slurm topology from the environment

tests/
└── schema_contract.rs   # Pins the NDJSON schema
```

## Implementation notes

### cgroup discovery

Neither `/proc/self/cgroup` nor `/proc/{tree_pid}/cgroup` is reliable alone, so
both are considered and ranked:

1. A candidate resolving to the **root** cgroup is rejected — root means the
   whole node, never a job.
2. A path naming the job wins. PBS writes `/pbs_jobs.service/jobid/12345...`
   and Slurm `/slurm/uid_1000/job_12345`.
3. Otherwise the more specific (deeper) path wins.

`--cgroup DIR` bypasses all of this if your site's layout is unusual.

Then, for whichever candidate won: prefer cgroup v1 `cpuacct` when present else
the v2 unified hierarchy, resolve controller mounts from `/proc/mounts` (falling
back to the conventional `/sys/fs/cgroup/*` layout), and walk up to the nearest
level where the controller is actually populated.

### Which processes count as the job's

By default the logger asks the kernel, reading `cgroup.procs` from the job's
cgroup and any child cgroups. This is the authoritative membership list and it
beats walking the process tree in four ways: it works where the job's processes
are not descendants of anything the logger started (every node but the head
one), it catches processes that reparent to init, it cannot pick up a
neighbour's processes on a shared node, and it is cheaper.

The tree walk from `--tree-pid` remains as a fallback for when no usable cgroup
is found. `--procs-from cgroup|tree|auto` forces the choice; the summary records
which was used in `proc_source`.

This is why remote loggers are launched with **no** `--tree-pid`. An earlier
version passed `--tree-pid 1`, which was wrong twice over: `/proc/1/cgroup` is
the root cgroup, so CPU and memory described the whole node rather than the job,
and a tree walk from init counted every process on the machine.

### CPU calculation

Measurement windows are contiguous: each tick's ending counter read becomes the
next tick's starting read, so CPU consumed between ticks is not lost.
Percentages use the *measured* elapsed time, which samples report as `dt_sec`.

Per-CPU vectors are indexed by **node CPU id** and sized to the node's CPU
count, not to the job's booked cores.

### Signal handling

Handles SIGINT, SIGTERM and SIGHUP. This matters: PBS sends **SIGTERM** on
walltime exhaustion and `qdel`, and a logger that ignores it dies without
writing `job_end` or the summary. A second signal exits immediately.

### Failure behaviour

Sampling errors back off and abort after 20 consecutive failures — the usual
cause is the job's cgroup being torn down, which means the job is over. The
output buffer is flushed at least every 2 seconds, bounding what a SIGKILL can
lose.

## Deployment

Users do not install anything — the tool is published as a module in a shared
project directory, so a colleague needs only the two `module` lines from the
Quick start. Handing people a binary to copy into `~/bin` does not scale past a
handful of users and leaves everyone on a different version.

To publish a new build into that shared location:

```bash
./build-linux.sh          # static musl binary, ~1.1 MB

# Stage the binary and the shell library together — they are versioned as a
# pair. The library invokes the binary by name from $PATH, so a module that puts
# one on the path without the other fails at job start with a confusing
# "not found".
scp target/x86_64-unknown-linux-musl/release/hpc-telemetry \
    hpc-telemetry.sh \
    gadi.nci.org.au:/g/data/gb02/hpc-telemetry/<version>/bin/
```

Then point the modulefile at the new version. Users pick the change up on their
next `module load` with nothing to reinstall.

The dashboard watcher picks up the output files automatically.

## Dependencies

**Runtime:** Linux with cgroup v1 or v2; glibc 2.31+ or musl; `nvidia-smi`
optional for GPU metrics.

**Build:** Rust 1.87+, declared as `rust-version` in `Cargo.toml` and enforced
by a CI job pinned to that exact toolchain. `clippy.toml` carries the same
number so clippy will not suggest APIs newer than the crate supports.

Only the build machine's toolchain matters — deployment is a static musl binary
with no Rust runtime dependency.
