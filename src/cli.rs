use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// How the logger decides which processes belong to the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProcsFrom {
    /// cgroup membership when available, process tree otherwise.
    Auto,
    /// Always the kernel's cgroup membership list.
    Cgroup,
    /// Always a walk from `--tree-pid`.
    Tree,
}

/// High-performance HPC telemetry logger (cgroup v1 and v2)
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Root PID of the job process tree (for RSS/process counting).
    /// Defaults to the parent process, which is the shell running the job script.
    #[arg(long, env = "TREE_PID", default_value_t = default_tree_pid())]
    pub tree_pid: i32,

    /// Sampling interval in seconds
    #[arg(long, default_value = "0.5", env = "INTERVAL")]
    pub interval: f64,

    /// How often to poll GPUs, in seconds. GPU utilisation does not need the
    /// same resolution as CPU, and each poll forks nvidia-smi.
    #[arg(long, default_value = "5.0", env = "GPU_INTERVAL")]
    pub gpu_interval: f64,

    /// Output NDJSON log file path. `--output` is accepted as an alias.
    ///
    /// Supports `{host}`, `{jobid}` and `{user}` placeholders, expanded on the
    /// node that does the writing. That lets one template be broadcast to every
    /// node of a multi-node job without the launcher having to know the
    /// hostnames — each logger fills in its own.
    ///
    /// Optional only so `--merge` can run without it; required otherwise.
    #[arg(long, visible_alias = "output", env = "OUTFILE")]
    pub outfile: Option<PathBuf>,

    /// Check that telemetry collection will work here, then exit.
    ///
    /// Reports what can and cannot be measured on this node — cgroup version
    /// and scope, which controllers are delegated to the job, whether the
    /// output path is writable — and exits non-zero only if the run would
    /// produce nothing usable. Meant to be run inside a short interactive job
    /// before committing a real one to the queue, since every failure it looks
    /// for otherwise shows up as a plausible-looking but wrong log file rather
    /// than as an error.
    #[arg(long)]
    pub check: bool,

    /// Merge the per-node summaries in DIR into one job-level summary, then
    /// exit. Used by the wrapper once every node's logger has stopped.
    #[arg(long, value_name = "DIR")]
    pub merge: Option<PathBuf>,

    /// Comma-separated list of the nodes the scheduler allocated. Used by
    /// `--merge` to report nodes that never produced a summary, and by
    /// `--write-manifest` to list the files each node will write.
    #[arg(long, value_name = "HOSTS")]
    pub merge_expect_nodes: Option<String>,

    /// Write the job manifest into DIR and exit.
    ///
    /// The manifest names every file the job will produce, so the dashboard can
    /// find them from the job id alone rather than globbing. Written by the
    /// wrapper at job start, before any logger runs.
    #[arg(long, value_name = "DIR")]
    pub write_manifest: Option<PathBuf>,

    /// This node's index within the job's allocation.
    #[arg(long, env = "TELEMETRY_NODE_RANK")]
    pub node_rank: Option<u32>,

    /// Use this cgroup directory instead of discovering one. An escape hatch for
    /// sites whose layout the discovery logic does not recognise.
    #[arg(long, value_name = "DIR")]
    pub cgroup: Option<PathBuf>,

    /// Where the set of processes to measure comes from.
    ///
    /// `cgroup` asks the kernel which processes belong to the job — correct on
    /// every node. `tree` walks descendants of --tree-pid, which only works
    /// where the job's processes are actually our descendants. `auto` uses the
    /// cgroup when one is available and falls back to the tree.
    #[arg(long, value_name = "SOURCE", default_value = "auto")]
    pub procs_from: ProcsFrom,

    /// Summary JSON file path (written on exit).
    /// Defaults to the output path with a `.summary.json` extension.
    #[arg(long, env = "SUMMARY")]
    pub summary: Option<PathBuf>,

    /// File containing the workload's exit status, written by the wrapper
    /// script before it shuts the logger down. Without it the logger has no way
    /// to know whether the job succeeded, and `exit_status` is reported as null
    /// rather than a misleading 0.
    #[arg(long, env = "EXIT_STATUS_FILE")]
    pub exit_status_file: Option<PathBuf>,

    /// User ID to record in telemetry
    #[arg(long, env = "USER")]
    pub user_id: Option<String>,

    /// Job ID to record in telemetry
    #[arg(long, env = "PBS_JOBID")]
    pub job_id: Option<String>,

    /// PBS queue name
    #[arg(long, default_value = "unknown", env = "PBS_QUEUE")]
    pub queue: String,

    /// PBS job name
    #[arg(long, default_value = "unknown", env = "PBS_JOBNAME")]
    pub job_name: String,

    /// PBS project/account
    #[arg(long, default_value = "unknown", env = "PBS_PROJECT")]
    pub project: String,

    /// Booked walltime (HH:MM:SS format).
    /// Checked from CLI, then PBS_RESOURCE_LIST_walltime (set via wrapper),
    /// then resolved at runtime from PBS_WALLTIME (seconds) or PBS_RESOURCE_walltime.
    #[arg(long, default_value = "", env = "PBS_RESOURCE_LIST_walltime")]
    pub booked_walltime: String,

    /// Booked memory (e.g., "4GB", "16000mb").
    /// Checked from CLI, then PBS_RESOURCE_LIST_mem, then PBS_RESOURCE_mem.
    #[arg(long, default_value = "", env = "PBS_RESOURCE_LIST_mem")]
    pub booked_mem: String,
}

fn default_tree_pid() -> i32 {
    #[cfg(unix)]
    {
        std::os::unix::process::parent_id() as i32
    }
    #[cfg(not(unix))]
    {
        std::process::id() as i32
    }
}

impl Args {
    /// Fill in values from PBS environment variables and validate.
    /// Call once after parsing.
    pub fn resolve_and_validate(&mut self) -> Result<()> {
        self.resolve_pbs_defaults();
        self.validate()
    }

    fn resolve_pbs_defaults(&mut self) {
        if self.booked_walltime.is_empty() {
            // PBS_WALLTIME is in seconds on some PBS Pro versions.
            if let Ok(val) = std::env::var("PBS_WALLTIME") {
                if let Ok(secs) = val.trim().parse::<u64>() {
                    self.booked_walltime = format_hms(secs);
                }
            }
        }
        if self.booked_walltime.is_empty() {
            if let Ok(val) = std::env::var("PBS_RESOURCE_walltime") {
                let v = val.trim().to_string();
                if !v.is_empty() {
                    self.booked_walltime = v;
                }
            }
        }
        if self.booked_mem.is_empty() {
            if let Ok(val) = std::env::var("PBS_RESOURCE_mem") {
                let v = val.trim().to_string();
                if !v.is_empty() {
                    self.booked_mem = v;
                }
            }
        }
        if self.project == "unknown" || self.project.is_empty() {
            for var in ["PBS_O_PROJECT", "PROJECT", "PBS_ACCOUNT"] {
                if let Ok(val) = std::env::var(var) {
                    let v = val.trim().to_string();
                    if !v.is_empty() {
                        self.project = v;
                        break;
                    }
                }
            }
        }
    }

    /// True when this invocation should report on the environment and exit.
    pub fn is_check_mode(&self) -> bool {
        self.check
    }

    /// True when this invocation should merge per-node summaries and exit
    /// rather than collect telemetry.
    pub fn is_merge_mode(&self) -> bool {
        self.merge.is_some()
    }

    /// True when this invocation should write the manifest and exit.
    pub fn is_manifest_mode(&self) -> bool {
        self.write_manifest.is_some()
    }

    /// The hosts to list in the manifest. Falls back to this node, so a
    /// single-node job still gets a manifest without the caller having to
    /// supply a node list.
    pub fn manifest_hosts(&self) -> Vec<String> {
        let hosts = self.expected_nodes();
        if hosts.is_empty() {
            vec![crate::host::hostname()]
        } else {
            hosts
        }
    }

    /// The nodes the scheduler allocated, for spotting ones that never reported.
    pub fn expected_nodes(&self) -> Vec<String> {
        self.merge_expect_nodes
            .as_deref()
            .map(|raw| {
                raw.split(',')
                    .map(|h| h.trim())
                    .filter(|h| !h.is_empty())
                    .map(|h| h.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The output path with placeholders expanded for this node.
    pub fn resolved_outfile(&self) -> Result<PathBuf> {
        let raw = self
            .outfile
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--output is required"))?;

        Ok(PathBuf::from(expand_placeholders(
            &raw.to_string_lossy(),
            &crate::host::hostname(),
            &self.get_job_id(),
            &self.get_user_id(),
        )))
    }

    fn validate(&self) -> Result<()> {
        // Checked before anything else, and deliberately exempt from the rest.
        // `--check` exists to be run by someone who has not set the tool up
        // yet — demanding --output before it will tell you whether the thing
        // can work at all would defeat the point. It reports on whatever it is
        // given, including nothing.
        if self.is_check_mode() {
            return Ok(());
        }

        if self.is_manifest_mode() {
            // Needs the output template to work out each node's filename, but
            // none of the sampling options apply.
            if self.outfile.is_none() {
                bail!(
                    "--write-manifest requires --output so the per-node filenames can be derived"
                );
            }
            return Ok(());
        }

        if self.is_merge_mode() {
            // Merge mode reads finished summaries; none of the sampling options
            // apply, but it does need to know which job to merge.
            if self.job_id.is_none() && std::env::var("PBS_JOBID").is_err() {
                bail!("--merge requires --job-id so the right summaries can be found");
            }
            return Ok(());
        }

        if self.outfile.is_none() {
            bail!("--output is required (or --outfile)");
        }

        // An interval of 0 previously made the tick deadline already-past: no
        // process stats were collected at all, every percentage was zero, and
        // the loop wrote samples as fast as the CPU allowed, filling gdata.
        // A negative interval panicked inside Duration::from_secs_f64.
        if !self.interval.is_finite() || self.interval < 0.1 {
            bail!(
                "--interval must be a finite value >= 0.1 seconds (got {}). \
                 Sub-100ms sampling costs more in /proc reads than it yields in resolution.",
                self.interval
            );
        }
        if self.interval > 3600.0 {
            bail!(
                "--interval of {}s is longer than an hour; that is almost certainly a mistake",
                self.interval
            );
        }
        if !self.gpu_interval.is_finite() || self.gpu_interval < self.interval {
            bail!(
                "--gpu-interval ({}) must be finite and at least --interval ({})",
                self.gpu_interval,
                self.interval
            );
        }
        if self.tree_pid <= 0 {
            bail!("--tree-pid must be a positive PID (got {})", self.tree_pid);
        }
        Ok(())
    }

    /// Summary path, defaulting to the resolved outfile with a
    /// `.summary.json` extension.
    pub fn summary_path(&self) -> PathBuf {
        if let Some(explicit) = &self.summary {
            return PathBuf::from(expand_placeholders(
                &explicit.to_string_lossy(),
                &crate::host::hostname(),
                &self.get_job_id(),
                &self.get_user_id(),
            ));
        }

        let outfile = self
            .resolved_outfile()
            .unwrap_or_else(|_| PathBuf::from("telemetry.log"));
        default_summary_path(&outfile)
    }

    /// Get user_id with fallback to current user
    pub fn get_user_id(&self) -> String {
        self.user_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()))
    }

    /// Get job_id with fallback
    pub fn get_job_id(&self) -> String {
        self.job_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Booked walltime in seconds
    pub fn booked_walltime_seconds(&self) -> u64 {
        parse_hms_to_seconds(&self.booked_walltime)
    }

    /// Booked memory in bytes
    pub fn booked_mem_bytes(&self) -> u64 {
        parse_mem_to_bytes(&self.booked_mem)
    }

    /// Booked walltime, normalised to HH:MM:SS.
    ///
    /// The same field used to be emitted as `""` in job_log and `"00:00:00"` in
    /// job_start; consumers had to special-case by event type.
    pub fn booked_walltime_normalised(&self) -> String {
        if self.booked_walltime.trim().is_empty() {
            "00:00:00".to_string()
        } else {
            self.booked_walltime.trim().to_string()
        }
    }

    /// Booked memory as the operator wrote it, e.g. `"4GB"`.
    ///
    /// Emitted alongside the numeric `booked_mem_bytes` so consumers never have
    /// to parse a human string. Previously job_start emitted `"4294967296b"`
    /// while job_log emitted `"4GB"` under the same field name.
    pub fn booked_mem_normalised(&self) -> String {
        if self.booked_mem.trim().is_empty() {
            "0b".to_string()
        } else {
            self.booked_mem.trim().to_string()
        }
    }
}

/// Expand `{host}`, `{jobid}` and `{user}` in an output path.
///
/// Every substitution is sanitised for filename use: a job id like
/// `12345.gadi-pbs` is fine, but nothing containing a `/` may be allowed to
/// redirect the output into another directory.
pub fn expand_placeholders(template: &str, host: &str, job_id: &str, user: &str) -> String {
    use crate::host::sanitise_for_filename;

    template
        .replace("{host}", &sanitise_for_filename(host))
        .replace("{jobid}", &sanitise_for_filename(job_id))
        .replace("{user}", &sanitise_for_filename(user))
}

/// Derive a summary path from an output path: `job.log` -> `job.log.summary.json`
fn default_summary_path(outfile: &std::path::Path) -> PathBuf {
    let mut name = outfile.file_name().unwrap_or_default().to_os_string();
    name.push(".summary.json");
    outfile.with_file_name(name)
}

fn format_hms(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Parse HH:MM:SS (or MM:SS, or SS) to seconds
fn parse_hms_to_seconds(hms: &str) -> u64 {
    let hms = hms.trim();
    if hms.is_empty() {
        return 0;
    }

    let parts: Vec<&str> = hms.split(':').collect();
    let nums: Vec<u64> = parts
        .iter()
        .map(|p| p.trim().parse::<u64>().unwrap_or(0))
        .collect();

    match nums.len() {
        3 => nums[0] * 3600 + nums[1] * 60 + nums[2],
        2 => nums[0] * 60 + nums[1],
        1 => nums[0],
        _ => 0,
    }
}

/// Parse memory strings like "4GB", "16000mb" to bytes
fn parse_mem_to_bytes(s: &str) -> u64 {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return 0;
    }

    let num_str: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let unit_str: String = s.chars().filter(|c| c.is_ascii_alphabetic()).collect();

    let value: f64 = num_str.parse().unwrap_or(0.0);

    let multiplier: u64 = match unit_str.as_str() {
        "b" | "" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        "t" | "tb" | "tib" => 1024_u64 * 1024 * 1024 * 1024,
        _ => 1,
    };

    (value * multiplier as f64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hms() {
        assert_eq!(parse_hms_to_seconds("01:30:00"), 5400);
        assert_eq!(parse_hms_to_seconds("00:10:30"), 630);
        assert_eq!(parse_hms_to_seconds("48:00:00"), 172800);
        assert_eq!(parse_hms_to_seconds("invalid"), 0);
        assert_eq!(parse_hms_to_seconds(""), 0);
        // PBS occasionally hands back MM:SS.
        assert_eq!(parse_hms_to_seconds("10:30"), 630);
    }

    #[test]
    fn test_parse_mem() {
        assert_eq!(parse_mem_to_bytes("4GB"), 4 * 1024 * 1024 * 1024);
        assert_eq!(parse_mem_to_bytes("16000mb"), 16000 * 1024 * 1024);
        assert_eq!(parse_mem_to_bytes("512kb"), 512 * 1024);
        assert_eq!(parse_mem_to_bytes(""), 0);
        assert_eq!(parse_mem_to_bytes("1024"), 1024);
        assert_eq!(parse_mem_to_bytes("  8gb  "), 8 * 1024 * 1024 * 1024);
        // PBS writes plain byte counts with a trailing b.
        assert_eq!(parse_mem_to_bytes("4294967296b"), 4294967296);
    }

    #[test]
    fn test_format_hms() {
        assert_eq!(format_hms(5400), "01:30:00");
        assert_eq!(format_hms(0), "00:00:00");
        assert_eq!(format_hms(172800), "48:00:00");
    }

    #[test]
    fn test_default_summary_path() {
        // Must not clobber the existing extension: psutil_123.log has to become
        // psutil_123.log.summary.json, not psutil_123.summary.json.
        assert_eq!(
            default_summary_path(std::path::Path::new("/g/data/psutil_123.log")),
            PathBuf::from("/g/data/psutil_123.log.summary.json")
        );
        assert_eq!(
            default_summary_path(std::path::Path::new("out")),
            PathBuf::from("out.summary.json")
        );
    }

    fn test_args() -> Args {
        Args {
            tree_pid: 1,
            interval: 0.5,
            gpu_interval: 5.0,
            outfile: Some(PathBuf::from("/tmp/out.log")),
            check: false,
            merge: None,
            merge_expect_nodes: None,
            write_manifest: None,
            node_rank: None,
            cgroup: None,
            procs_from: ProcsFrom::Auto,
            summary: None,
            exit_status_file: None,
            user_id: None,
            job_id: None,
            queue: "normal".into(),
            job_name: "job".into(),
            project: "proj".into(),
            booked_walltime: String::new(),
            booked_mem: String::new(),
        }
    }

    #[test]
    fn test_validate_rejects_bad_interval() {
        let mut args = test_args();

        args.interval = 0.0;
        assert!(args.validate().is_err(), "zero interval must be rejected");

        args.interval = -1.0;
        assert!(
            args.validate().is_err(),
            "negative interval must be rejected"
        );

        args.interval = f64::NAN;
        assert!(args.validate().is_err(), "NaN interval must be rejected");

        args.interval = 0.01;
        assert!(
            args.validate().is_err(),
            "sub-100ms interval must be rejected"
        );

        args.interval = 100_000.0;
        assert!(args.validate().is_err(), "absurd interval must be rejected");

        args.interval = 0.5;
        assert!(args.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_bad_pid_and_gpu_interval() {
        let mut args = test_args();

        args.tree_pid = 0;
        assert!(args.validate().is_err());
        args.tree_pid = -5;
        assert!(args.validate().is_err());
        args.tree_pid = 1;

        args.gpu_interval = 0.1;
        args.interval = 0.5;
        assert!(
            args.validate().is_err(),
            "gpu interval below sample interval is pointless"
        );
    }

    #[test]
    fn test_booked_normalisation_is_event_independent() {
        let mut args = test_args();

        // Empty booking: both events must agree on the same placeholder.
        assert_eq!(args.booked_walltime_normalised(), "00:00:00");
        assert_eq!(args.booked_mem_normalised(), "0b");
        assert_eq!(args.booked_mem_bytes(), 0);

        args.booked_walltime = "02:00:00".into();
        args.booked_mem = "4GB".into();
        assert_eq!(args.booked_walltime_normalised(), "02:00:00");
        assert_eq!(args.booked_walltime_seconds(), 7200);
        assert_eq!(args.booked_mem_normalised(), "4GB");
        assert_eq!(args.booked_mem_bytes(), 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn test_summary_path_defaults_from_outfile() {
        let args = test_args();
        assert_eq!(
            args.summary_path(),
            PathBuf::from("/tmp/out.log.summary.json")
        );
    }

    #[test]
    fn test_cli_parses_documented_invocation() {
        // This is the exact shape of the command in the README. It used to fail
        // because the README said --output while the CLI only accepted
        // --outfile, and because --summary and --tree-pid were required.
        let args = Args::try_parse_from([
            "hpc-telemetry",
            "--output",
            "/g/data/ab12/dashboard/user/psutil_123.log",
            "--interval",
            "0.5",
            "--user-id",
            "sam",
            "--job-id",
            "123.gadi-pbs",
        ]);

        let args = args.expect("documented invocation must parse");
        assert_eq!(args.get_user_id(), "sam");
        assert_eq!(args.get_job_id(), "123.gadi-pbs");
        assert_eq!(
            args.outfile,
            Some(PathBuf::from("/g/data/ab12/dashboard/user/psutil_123.log"))
        );

        // Deliberately no assertion on summary_path() here. `--summary` carries
        // `env = "SUMMARY"`, so on a machine that happens to export SUMMARY this
        // parse would pick it up and the assertion would fail for reasons that
        // have nothing to do with the code under test. The default-derivation
        // logic is covered env-independently by
        // test_summary_path_defaults_from_outfile and test_default_summary_path,
        // which build Args directly.
    }

    #[test]
    fn test_cli_accepts_outfile_spelling_too() {
        let args = Args::try_parse_from(["hpc-telemetry", "--outfile", "/tmp/x.log"])
            .expect("--outfile must still work");
        assert_eq!(args.outfile, Some(PathBuf::from("/tmp/x.log")));
    }

    // --- multi-node ---

    #[test]
    fn test_expand_placeholders() {
        assert_eq!(
            expand_placeholders(
                "/g/data/ab12/dashboard/{user}/psutil_{jobid}_{host}.log",
                "gadi-cpu-clx-0123",
                "12345.gadi-pbs",
                "sam"
            ),
            "/g/data/ab12/dashboard/sam/psutil_12345.gadi-pbs_gadi-cpu-clx-0123.log"
        );

        // A template with no placeholders is left alone, so single-node
        // invocations behave exactly as before.
        assert_eq!(
            expand_placeholders("/tmp/plain.log", "host", "job", "user"),
            "/tmp/plain.log"
        );
    }

    #[test]
    fn test_expand_placeholders_cannot_escape_the_directory() {
        // A hostname or job id containing a slash must not be able to redirect
        // output somewhere else on the filesystem.
        let expanded = expand_placeholders("/g/data/out_{host}.log", "../../etc/evil", "j", "u");
        assert!(!expanded.contains(".."), "got {expanded}");
    }

    #[test]
    fn test_resolved_outfile_expands_per_node() {
        let mut args = test_args();
        args.job_id = Some("12345.gadi-pbs".into());
        args.user_id = Some("sam".into());
        args.outfile = Some(PathBuf::from("/g/data/{user}/psutil_{jobid}_{host}.log"));

        let resolved = args.resolved_outfile().unwrap();
        let text = resolved.to_string_lossy();

        // The hostname is whatever this machine is called, so assert on shape.
        assert!(text.starts_with("/g/data/sam/psutil_12345.gadi-pbs_"));
        assert!(text.ends_with(".log"));
        assert!(!text.contains('{'), "no placeholder should survive: {text}");
    }

    #[test]
    fn test_summary_path_follows_the_expanded_outfile() {
        let mut args = test_args();
        args.job_id = Some("12345".into());
        args.outfile = Some(PathBuf::from("/g/data/psutil_{jobid}_{host}.log"));

        let summary = args.summary_path();
        let text = summary.to_string_lossy();

        // Each node must land on its own summary file, or they overwrite one
        // another and the merge sees a single node.
        assert!(text.ends_with(".log.summary.json"), "got {text}");
        assert!(text.contains("psutil_12345_"));
        assert!(!text.contains('{'));
    }

    #[test]
    fn test_merge_mode_does_not_require_an_output_path() {
        let mut args = test_args();
        args.outfile = None;
        args.merge = Some(PathBuf::from("/g/data/dashboard/sam"));
        args.job_id = Some("12345.gadi-pbs".into());

        assert!(args.is_merge_mode());
        assert!(
            args.validate().is_ok(),
            "merge reads finished summaries; it has nothing to write telemetry to"
        );
    }

    #[test]
    fn test_non_merge_mode_still_requires_an_output_path() {
        let mut args = test_args();
        args.outfile = None;
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_manifest_mode_needs_only_an_output_template() {
        let mut args = test_args();
        args.write_manifest = Some(PathBuf::from("/g/data/dashboard/sam"));

        assert!(args.is_manifest_mode());
        assert!(args.validate().is_ok());

        // The template is what per-node filenames are derived from, so it is
        // the one thing manifest mode cannot do without.
        args.outfile = None;
        assert!(args.validate().is_err());
    }

    #[test]
    fn test_manifest_hosts_falls_back_to_this_node() {
        let mut args = test_args();

        // Single-node job: no node list supplied, so the manifest still names
        // one host rather than being empty.
        let hosts = args.manifest_hosts();
        assert_eq!(hosts.len(), 1);
        assert!(!hosts[0].is_empty());

        args.merge_expect_nodes = Some("node1,node2,node3".into());
        assert_eq!(args.manifest_hosts().len(), 3);
    }

    #[test]
    fn test_expected_nodes_parsing() {
        let mut args = test_args();
        assert!(args.expected_nodes().is_empty());

        args.merge_expect_nodes = Some(" node1, node2 ,,node3 ".into());
        assert_eq!(
            args.expected_nodes(),
            vec![
                "node1".to_string(),
                "node2".to_string(),
                "node3".to_string()
            ]
        );
    }
}
