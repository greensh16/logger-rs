# Changelog

Notable changes to logger-rs. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

Releases before 0.3.1 predate this file; see the
[releases page](https://github.com/greensh16/logger-rs/releases) for those.

## [0.3.1] — unreleased

A job killed at its walltime limit used to lose its job-level summary entirely.
This release fixes that, and lays the groundwork for Slurm.

### Fixed

- **A walltime kill no longer destroys the job summary.** Each node now
  checkpoints its summary during the run rather than only at exit, so a
  `SIGKILL` — which cannot be caught, and is how the scheduler ends a job that
  exhausts its walltime — leaves something to merge. Previously every node in
  such a job wrote nothing at all and `--merge` failed outright, even though the
  NDJSON stream beside it held the whole run.
- **`telemetry_stop` no longer waits itself to death.** When the job is being
  killed (exit status 143 or 137) it waits 4 seconds for the remote loggers
  instead of 30. PBS Pro's grace period before `SIGKILL` is `kill_delay`, ten
  seconds by default, so the old unconditional 30-second wait guaranteed the
  merge was killed before it ran. It did not produce more summaries; it produced
  none.
- Clippy `needless_borrows_for_generic_args` errors in the Slurm environment
  helpers, which failed the build under `-D warnings`.

### Added

- `--summary-every SECS` (default `30`, `0` disables), on both the binary and
  the shell library, controlling the checkpoint period above. Each checkpoint is
  one small write-and-rename per node.
- `partial` on the per-node summary: `true` while the file is a checkpoint,
  cleared by the final write, so a summary always says whether it describes a
  complete run.
- `nodes_partial` on the merged summary, and `partial` per entry in `per_node`,
  listing nodes that reported but did not reach a clean finish. Distinct from
  `nodes_missing`, which is for nodes that never reported at all: a partial
  node's figures are a lower bound rather than absent. The dashboard shows this
  as its own banner.
- Slurm job identity and booked resources — job id, partition, job name,
  account, walltime and memory are read from `SLURM_*` when PBS supplies
  nothing. On Setonix, booked walltime falls back to the exported job start
  and end timestamps.
- Slurm cgroup v2 job-root discovery and a multi-node `srun` launch path.
  The latter handles Setonix's conflicting inherited CPU settings without
  changing the workload's environment.
- `testing/probe_setonix.slurm`, a read-only diagnostic job that reports a
  Slurm site's cgroup layout and GPU tooling, and
  `testing/test_telemetry.slurm`, a two-node measured CPU test.

### Changed

- A failed `--merge` now distinguishes its two causes. NDJSON logs present means
  the loggers ran and only the end-of-run write was lost — the samples are
  intact and the dashboard can still open the job. No logs at all means the
  loggers never started, which is a job-script problem. The same message used to
  cover both.
- The shell library prints the exact `logger-rs --merge …` command to retry with
  after a failed merge.
- The DOI badge and `CITATION.cff` now use the Zenodo **concept** DOI
  (`10.5281/zenodo.22105313`) instead of the v0.3.0 version DOI. The concept DOI
  resolves to the newest release, so it does not go stale — and a version DOI
  cannot be committed anyway, since Zenodo only mints it after the tag is
  pushed.
- README: CI, release, licence, latest-release and Rust-version badges, and a
  support section.

### Validation

- Setonix `work` partition CPU telemetry passed a two-node end-to-end test
  (job 51291742, 2026-10-21): both nodes reported from the job cgroup and
  recorded 39.38 core-seconds for a 40 core-second workload. Job-level memory
  counters were readable, but memory accuracy under load was not tested.
  Other Slurm sites and GPU partitions have not been validated.

### Known limitations

- Setonix does not expose job-level cgroup I/O at the observed job root, and
  cgroup v2 does not expose per-core CPU breakdown. Network counters describe
  each whole node rather than only this job.
- AMD GPUs (ROCm) are not supported. GPU metrics remain NVIDIA-only.
- A checkpointed node's totals are a lower bound, understated by up to one
  checkpoint period. The NDJSON stream runs closer to the true end of the job;
  reconstructing exact totals from it is not implemented.

[0.3.1]: https://github.com/greensh16/logger-rs/compare/v0.3.0...v0.3.1
