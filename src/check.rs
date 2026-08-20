//! Preflight check: `hpc-telemetry --check`.
//!
//! Answers "will this actually collect anything if I submit the job?" in a few
//! seconds, inside an interactive session, instead of after a queue wait.
//!
//! Every problem this reports has been hit for real:
//!
//! - `blkio` not delegated to the job on Gadi, so disk I/O silently reported
//!   the PBS daemon's lifetime totals rather than the job's — 200 GB read for
//!   800 MB of actual work.
//! - `--tree-pid 1` on a remote node resolving to the *root* cgroup, which
//!   would have reported the whole node's CPU and memory as the job's.
//! - An output directory on `/g/data` that the job never requested via
//!   `-l storage=`, so every write failed once the job started.
//!
//! None of those announce themselves. Each produces a plausible-looking log
//! file that is quietly wrong, which is worse than a crash.
//!
//! Exit status is deliberately narrow: non-zero only when the run would produce
//! *nothing usable*. A missing controller costs you one metric and is reported
//! as a warning, because a job that measures CPU but not disk is still worth
//! running — failing the check there would train people to ignore it.

use crate::cgroup::{Cgroup, CgroupVersion};
use crate::cli::Args;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

/// One line of the report.
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn tag(&self) -> &'static str {
        match self {
            Status::Ok => "  ok  ",
            Status::Warn => " warn ",
            Status::Fail => " FAIL ",
        }
    }
}

struct Report {
    lines: Vec<(Status, String, String)>,
}

impl Report {
    fn new() -> Self {
        Report { lines: Vec::new() }
    }

    fn ok(&mut self, what: &str, detail: impl Into<String>) {
        self.lines
            .push((Status::Ok, what.to_string(), detail.into()));
    }

    fn warn(&mut self, what: &str, detail: impl Into<String>) {
        self.lines
            .push((Status::Warn, what.to_string(), detail.into()));
    }

    fn fail(&mut self, what: &str, detail: impl Into<String>) {
        self.lines
            .push((Status::Fail, what.to_string(), detail.into()));
    }

    fn has_failures(&self) -> bool {
        self.lines.iter().any(|(s, _, _)| matches!(s, Status::Fail))
    }

    fn warn_count(&self) -> usize {
        self.lines
            .iter()
            .filter(|(s, _, _)| matches!(s, Status::Warn))
            .count()
    }

    fn render(&self) -> String {
        let width = self
            .lines
            .iter()
            .map(|(_, what, _)| what.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for (status, what, detail) in &self.lines {
            let _ = writeln!(out, "[{}] {:width$}  {}", status.tag(), what, detail);
        }
        out
    }
}

/// Run every check and print the report. Returns the process exit code.
pub fn run(args: &Args) -> i32 {
    let mut r = Report::new();

    println!("hpc-telemetry preflight check");
    println!("{}", "-".repeat(72));

    check_scheduler_env(&mut r);
    let cgroup = check_cgroup(&mut r, args);
    check_controllers(&mut r, cgroup.as_ref());
    check_output(&mut r, args);
    check_gpu(&mut r);

    print!("{}", r.render());
    println!("{}", "-".repeat(72));

    if r.has_failures() {
        println!(
            "Not ready: the failures above would stop this collecting anything usable.\n\
             Fix those and run --check again before submitting."
        );
        1
    } else if r.warn_count() > 0 {
        println!(
            "Ready, with {} warning(s). Those metrics will be missing but everything\n\
             else will be collected — this is normal on some sites.",
            r.warn_count()
        );
        0
    } else {
        println!("Ready. Everything this can measure is available here.");
        0
    }
}

fn check_scheduler_env(r: &mut Report) {
    let jobid = std::env::var("PBS_JOBID")
        .ok()
        .or_else(|| std::env::var("SLURM_JOB_ID").ok());

    match jobid {
        Some(id) => r.ok("scheduler", format!("job {id}")),
        None => r.warn(
            "scheduler",
            "no PBS_JOBID/SLURM_JOB_ID — not inside a job. Results will be labelled \
             'local'; run this inside an interactive job to check the real thing.",
        ),
    }

    // Node count, and the Gadi gotcha that NCPUS is per chunk rather than for
    // the whole allocation.
    if let Ok(nodefile) = std::env::var("PBS_NODEFILE") {
        match fs::read_to_string(&nodefile) {
            Ok(contents) => {
                let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
                let mut hosts: Vec<&str> = lines.clone();
                hosts.sort_unstable();
                hosts.dedup();
                r.ok(
                    "allocation",
                    format!(
                        "{} node(s), {} core(s) total ({})",
                        hosts.len(),
                        lines.len(),
                        hosts.join(", ")
                    ),
                );
                if hosts.len() > 1 {
                    r.ok(
                        "multi-node",
                        "the wrapper will start a logger on every node and merge the results",
                    );
                }
            }
            Err(e) => r.warn(
                "allocation",
                format!("PBS_NODEFILE set to {nodefile} but unreadable: {e}"),
            ),
        }
    } else if std::env::var("SLURM_JOB_NODELIST").is_ok() {
        r.ok("allocation", "SLURM_JOB_NODELIST present");
    }

    check_booked_walltime(r);
}

/// Whether the booked walltime can be determined at all.
///
/// PBS does not put it in the environment on Gadi — none of
/// `PBS_RESOURCE_LIST_walltime`, `PBS_WALLTIME` or `PBS_RESOURCE_walltime` are
/// exported, the same gap that forced booked *memory* to be read from the
/// cgroup. Unlike memory there is no kernel-side source, so the scheduler is
/// the only place it exists and `hpc-telemetry.sh` asks `qstat` for it.
///
/// Worth reporting because the failure is silent and the consequence is subtle:
/// without it `booked_walltime_sec` is 0, and "used 5% of your booking" becomes
/// indistinguishable from "we never found out what you booked".
fn check_booked_walltime(r: &mut Report) {
    for var in [
        "PBS_RESOURCE_LIST_walltime",
        "PBS_WALLTIME",
        "PBS_RESOURCE_walltime",
    ] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                r.ok("booked walltime", format!("{} = {}", var, v.trim()));
                return;
            }
        }
    }

    if std::env::var("PBS_JOBID").is_err() {
        return; // Not in a job; already reported by the scheduler check.
    }

    let qstat_on_path = std::env::var("PATH")
        .is_ok_and(|p| std::env::split_paths(&p).any(|d| d.join("qstat").is_file()));

    if qstat_on_path {
        r.ok(
            "booked walltime",
            "not in the environment (normal on Gadi), but qstat is available — \
             hpc-telemetry.sh will read it from the scheduler",
        );
    } else {
        r.warn(
            "booked walltime",
            "not in the environment and qstat is not on PATH, so it cannot be determined. \
             Walltime efficiency will be reported as unknown rather than guessed at.",
        );
    }
}

fn check_cgroup(r: &mut Report, args: &Args) -> Option<Cgroup> {
    let job_id = args.get_job_id();
    let tree_pid = std::process::id() as i32;

    match Cgroup::discover(tree_pid, &job_id) {
        Ok(cg) => {
            let version = match cg.version {
                CgroupVersion::V1 => "v1",
                CgroupVersion::V2 => "v2",
            };

            // The root cgroup means "the whole node", not "this job". Reporting
            // it would attribute every other job on the machine to this one.
            if cg.rel_path.is_empty() || cg.rel_path == "/" {
                r.fail(
                    "cgroup",
                    format!(
                        "resolved to the ROOT cgroup (v{version}, via {}). That measures the \
                         entire node, not your job. Telemetry would be badly wrong rather than \
                         merely incomplete.",
                        cg.source
                    ),
                );
            } else {
                r.ok(
                    "cgroup",
                    format!("{version} at {} (via {})", cg.rel_path, cg.source),
                );
            }

            if matches!(cg.version, CgroupVersion::V2) {
                r.warn(
                    "per-core CPU",
                    "cgroup v2 exposes no per-CPU breakdown, so the dashboard's Cores tab \
                     and the 'idle cores vs poor scaling' advice will be unavailable. \
                     Job-level CPU totals are unaffected.",
                );
            }

            Some(cg)
        }
        Err(e) => {
            r.fail(
                "cgroup",
                format!(
                    "could not discover one: {e}. Without a cgroup nothing job-scoped can be \
                     measured. On a non-Linux host (a login node on macOS, say) this is expected."
                ),
            );
            None
        }
    }
}

fn check_controllers(r: &mut Report, cgroup: Option<&Cgroup>) {
    let Some(cg) = cgroup else {
        return;
    };

    match cg.read_cpu_usage() {
        Ok(usage) => {
            let percpu = if usage.percpu_ns.is_empty() {
                String::from("no per-core breakdown")
            } else {
                format!("{} cores visible", usage.percpu_ns.len())
            };
            r.ok("cpu accounting", format!("readable, {percpu}"));
        }
        Err(e) => r.fail(
            "cpu accounting",
            format!("unreadable: {e}. CPU usage is the core metric; without it there is little point collecting."),
        ),
    }

    match cg.read_memory() {
        Some(mem) => {
            let limit = match mem.limit_bytes {
                Some(b) => format!("booked {:.1} GB", b as f64 / 1024.0_f64.powi(3)),
                None => String::from(
                    "no limit set — memory will be reported in GB, not as a \
                                      fraction of what you booked",
                ),
            };
            let peak = if mem.peak_bytes.is_some() {
                ", kernel high-water mark available"
            } else {
                ", no kernel high-water mark (peaks are sampled, so a short spike can be missed)"
            };
            r.ok("memory accounting", format!("readable, {limit}{peak}"));
        }
        None => r.warn(
            "memory accounting",
            "unreadable — memory will fall back to summing RSS across processes, which \
             double-counts shared pages.",
        ),
    }

    // The one that bit hardest in testing: present, readable, and reporting
    // somebody else's numbers.
    match cg.read_io() {
        Some(io) => r.ok(
            "disk I/O accounting",
            format!(
                "job-scoped and readable ({} read / {} written so far)",
                human_bytes(io.read_bytes),
                human_bytes(io.write_bytes)
            ),
        ),
        None => r.warn(
            "disk I/O accounting",
            "not delegated to this job — disk read/write will be absent. This is the case \
             on Gadi. Deliberately reported as missing rather than showing the PBS daemon's \
             lifetime totals, which is what an earlier version did.",
        ),
    }

    let (cpus, source) = cg.allowed_cpus();
    if cpus.is_empty() {
        r.warn(
            "core count",
            "could not determine which cores this job holds; efficiency percentages will \
             be unreliable.",
        );
    } else {
        r.ok(
            "core count",
            format!("{} core(s) on this node (via {source})", cpus.len()),
        );
    }
}

fn check_output(r: &mut Report, args: &Args) {
    let Some(outfile) = args.outfile.as_ref() else {
        r.warn(
            "output path",
            "none given. The wrapper defaults to $HPC_DASHBOARD_DIR or the current \
             directory; pass --output here to check a specific path.",
        );
        return;
    };

    // Placeholders are expanded per node at write time, so check the directory
    // rather than the templated filename.
    let shown = outfile.display().to_string();
    let dir = outfile.parent().unwrap_or(Path::new("."));

    if !dir.exists() {
        match fs::create_dir_all(dir) {
            Ok(()) => r.ok("output directory", format!("{} (created)", dir.display())),
            Err(e) => {
                r.fail(
                    "output directory",
                    format!(
                        "cannot create {}: {e}. If this is on /g/data or /scratch, the job \
                         needs -l storage=gdata/<proj> (or scratch/<proj>) — without it the \
                         filesystem is not mounted inside the job at all.",
                        dir.display()
                    ),
                );
                return;
            }
        }
    }

    // Existence is not permission: a directory can be listable and not writable.
    let probe = dir.join(format!(".hpc-telemetry-check-{}", std::process::id()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            r.ok("output writable", shown);
        }
        Err(e) => r.fail(
            "output writable",
            format!(
                "{} exists but is not writable: {e}. Check permissions, and that the job \
                 requested this filesystem with -l storage=.",
                dir.display()
            ),
        ),
    }

    if is_probably_unshared(dir) {
        r.warn(
            "output location",
            format!(
                "{} looks node-local. On a multi-node job each node would write to its own \
                 private copy and the merge would find only one. Prefer /g/data or /scratch.",
                dir.display()
            ),
        );
    }
}

/// `/tmp`, `$PBS_JOBFS` and the like are per-node, so a multi-node job writing
/// there produces summaries the merge step can never see.
fn is_probably_unshared(dir: &Path) -> bool {
    let s = dir.to_string_lossy();
    s.starts_with("/tmp") || s.starts_with("/var/tmp") || s.starts_with("/jobfs")
}

fn check_gpu(r: &mut Report) {
    let found = std::env::var("PATH")
        .is_ok_and(|path| std::env::split_paths(&path).any(|p| p.join("nvidia-smi").is_file()));

    if found {
        r.ok("gpu", "nvidia-smi present, GPU metrics will be collected");
    } else {
        r.ok(
            "gpu",
            "no nvidia-smi — GPU metrics skipped, which is correct on a CPU-only node",
        );
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_exit_logic() {
        let mut r = Report::new();
        r.ok("a", "fine");
        assert!(!r.has_failures());
        assert_eq!(r.warn_count(), 0);

        r.warn("b", "missing metric");
        assert!(!r.has_failures(), "a warning must not fail the check");
        assert_eq!(r.warn_count(), 1);

        r.fail("c", "broken");
        assert!(r.has_failures());
    }

    #[test]
    fn node_local_paths_are_flagged() {
        assert!(is_probably_unshared(Path::new("/tmp/logs")));
        assert!(is_probably_unshared(Path::new("/jobfs/12345")));
        assert!(!is_probably_unshared(Path::new("/g/data/gb02/logs")));
        assert!(!is_probably_unshared(Path::new("/scratch/gb02/logs")));
    }

    #[test]
    fn report_renders_aligned() {
        let mut r = Report::new();
        r.ok("cgroup", "v1");
        r.warn("disk I/O accounting", "absent");
        let out = r.render();
        assert!(out.contains("[  ok  ]"));
        assert!(out.contains("[ warn ]"));

        // The longest label sets the column, so every detail starts at the same
        // offset. Asserted by finding the details rather than by counting
        // spaces in a literal, which silently rots the moment a label changes.
        let cols: Vec<usize> = out
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.find("v1").or_else(|| l.find("absent")).unwrap())
            .collect();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], cols[1], "detail columns should line up");
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0.0 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0 GB");
    }
}
