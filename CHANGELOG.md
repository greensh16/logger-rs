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
  nothing. **This is not yet Slurm support**: cgroup handling and the shell
  library's launch path are still PBS-only. See *Known limitations*.
- `testing/probe_setonix.slurm`, a read-only diagnostic job that reports a
  Slurm site's cgroup layout and GPU tooling. It measures nothing and changes
  nothing; its output is what the remaining Slurm work will be written against.

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

### Known limitations

- Slurm is **not** supported end to end. Only job identity and booked resources
  read from the environment; the cgroup layer and the multi-node launch path
  still assume PBS. Do not expect a Slurm job to produce useful telemetry yet.
- AMD GPUs (ROCm) are not supported. GPU metrics remain NVIDIA-only.
- A checkpointed node's totals are a lower bound, understated by up to one
  checkpoint period. The NDJSON stream runs closer to the true end of the job;
  reconstructing exact totals from it is not implemented.

[0.3.1]: https://github.com/greensh16/logger-rs/compare/v0.3.0...v0.3.1
