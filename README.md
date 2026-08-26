# logger-rs

Resource telemetry for HPC jobs, written in Rust.

`logger-rs` samples a job's usage from its cgroup and writes NDJSON to shared
storage. The [HPC Dashboard](https://github.com/greensh16/hpc_dashboard_public)
reads those files directly — no daemon, no HTTP, no database.

Supports **cgroup v1** (Gadi today) and **cgroup v2** (unified hierarchy, what a
RHEL 9 refresh would bring). The hierarchy in use is detected at startup and
recorded in the output.

📖 **[Full documentation is in the wiki](https://github.com/greensh16/logger-rs/wiki)**

## Quick start

Put two lines around the part of your job script you want measured:

```bash
module use /g/data/gb02/modules
module load logger-rs
source logger-rs.sh

./stage-inputs.sh                            # setup — not measured

telemetry_start
    mpirun -np "$PBS_NCPUS" ./my_model       # measured
    ./postprocess                            # measured
telemetry_stop

./copy-results-back.sh                       # clean-up — not measured
```

Your script keeps its shape. Anything can go between `telemetry_start` and
`telemetry_stop` — pipelines, redirections, several launches, a command line
assembled in variables. None of those survive being prefixed with a launcher,
which is why that approach was dropped.

Marking a region rather than the whole script also keeps **data staging out of
the numbers**. A twenty-minute copy from `/g/data` to jobfs, measured, looks
like your job running at nearly zero CPU efficiency: it drags every figure down
and makes the dashboard recommend fewer cores than the science actually needs.

On a multi-node job, `telemetry_start` launches a logger on **every node** and
`telemetry_stop` merges their summaries into one job-level file.

A complete, commented job script is in [`examples/gadi-job.pbs`](examples/gadi-job.pbs)
— copy it and change four lines.

## Check it will work before you queue

```bash
logger-rs --check
```

Run inside a short interactive job. It reports the cgroup version and scope,
which controllers the job actually owns, whether the output path is writable,
and what will therefore be missing — in seconds, rather than after a queue wait.

Everything it looks for otherwise produces a plausible-looking log file that is
quietly wrong, which is much harder to notice than a crash.

## Nothing here can fail your job

A missing binary, an unwritable output directory, a logger that dies on
startup: each is reported and then ignored, and `telemetry_start` always returns
success. Under `set -e` — which most production scripts use — anything else
would abort the job over its own instrumentation. Running unmeasured is far
better than not running.

## Documentation

| Page | |
|---|---|
| [Quick Start](https://github.com/greensh16/logger-rs/wiki/Quick-Start) | Install, the two lines, a complete job script, how `source` finds the file |
| [Troubleshooting](https://github.com/greensh16/logger-rs/wiki/Troubleshooting) | Things that look like faults but aren't, and the ones that are |
| [Metrics](https://github.com/greensh16/logger-rs/wiki/Metrics) | What is collected, from where, and each figure's *scope* |
| [CLI Reference](https://github.com/greensh16/logger-rs/wiki/CLI-Reference) | Every argument, and the NDJSON output format |
| [Multi-Node Jobs](https://github.com/greensh16/logger-rs/wiki/Multi-Node-Jobs) | File layout, the manifest, what the merge can and cannot combine |
| [Dashboard Integration](https://github.com/greensh16/logger-rs/wiki/Dashboard-Integration) | Reading a job programmatically, schema stability |
| [Architecture](https://github.com/greensh16/logger-rs/wiki/Architecture) | Module map and the implementation decisions worth knowing |
| [Building and Deployment](https://github.com/greensh16/logger-rs/wiki/Building-and-Deployment) | Building, cross-compiling, releases, publishing the module |

The wiki is a separate repository, cloned alongside this one as
`logger-rs.wiki/` so documentation changes can be made in the same sitting
as the code that motivated them.

## Building

```bash
cargo build --release
cargo test
```

Cross-compiling a static Linux binary from macOS:

```bash
brew install filosottile/musl-cross/musl-cross
rustup target add x86_64-unknown-linux-musl
./build-linux.sh
```

Produces a ~1.1 MB fully static binary that runs on any Linux x86_64 host. The
binary also builds and runs on macOS for development, but with no cgroups
available it runs in stub mode and collects nothing.

**Requires** Rust 1.87+. Only the build machine's toolchain matters —
deployment is a static musl binary with no Rust runtime dependency.

## Licence

Apache License 2.0 — see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).

Copyright 2026 Sam Green. Developed at the ARC Centre of Excellence for 21st
Century Weather, UNSW Sydney.

If you use this in published work, please cite it — see
[`CITATION.cff`](CITATION.cff), or the *Cite this repository* button in the
sidebar.
