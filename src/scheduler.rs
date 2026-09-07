//! Scheduler (PBS / Slurm) metadata from the environment.

/// Scheduler metadata for multi-node jobs
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerMetadata {
    pub num_nodes: Option<u32>,
    pub tasks_per_node: Option<u32>,
    pub cpus_per_task: Option<u32>,
}

/// Read scheduler metadata from the process environment.
pub fn get_scheduler_metadata() -> SchedulerMetadata {
    scheduler_metadata_from(|key| std::env::var(key).ok())
}

/// Pure form of [`get_scheduler_metadata`], parameterised over the variable
/// lookup.
///
/// Splitting this out is what makes the tests deterministic: they used to call
/// `env::set_var`/`remove_var`, which mutate *process-global* state while cargo
/// runs tests in parallel threads, so one test could clear the variables
/// another had just set. (`env::set_var` is also `unsafe` as of edition 2024.)
pub fn scheduler_metadata_from<F>(get: F) -> SchedulerMetadata
where
    F: Fn(&str) -> Option<String>,
{
    let num = |key: &str| -> Option<u32> { get(key)?.trim().parse::<u32>().ok() };

    // --- PBS ---
    let pbs_nodes = num("PBS_NUM_NODES").or_else(|| num("PBS_NNODES"));

    // NCPUS is *per chunk*, not the whole allocation.
    //
    // A 96-CPU job across two Gadi nodes has NCPUS=48, so dividing by the node
    // count reported 24 CPUs per node for a job that had 48. PBS has no notion
    // of "tasks" anyway, so nothing here derives cpus_per_task — the caller
    // falls back to the cpuset size, which is measured rather than inferred and
    // was correct all along.
    let mut meta = SchedulerMetadata {
        num_nodes: pbs_nodes,
        tasks_per_node: None,
        cpus_per_task: None,
    };

    // --- Slurm (overrides PBS when both are present) ---
    if let Some(n) = num("SLURM_JOB_NUM_NODES").or_else(|| num("SLURM_NNODES")) {
        meta.num_nodes = Some(n);
    }
    if let Some(n) = num("SLURM_NTASKS_PER_NODE") {
        meta.tasks_per_node = Some(n);
    }
    if let Some(n) = num("SLURM_CPUS_PER_TASK") {
        meta.cpus_per_task = Some(n);
    }

    meta
}

// ---------------------------------------------------------------------------
// Slurm job identity and booked resources
//
// PBS gets these through clap's `env = "PBS_..."` attributes, one variable per
// argument. Slurm cannot use that mechanism because several of these need a
// *fallback chain* or a unit conversion, so they are resolved here and applied
// only where the PBS path left a value empty or "unknown".
// ---------------------------------------------------------------------------

/// Identity fields a scheduler can supply. `None` means "this scheduler did
/// not tell us", which is different from "unknown" — the caller only fills in
/// fields it has nothing better for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobIdentity {
    pub job_id: Option<String>,
    /// Slurm's partition is the closest analogue of a PBS queue.
    pub queue: Option<String>,
    pub job_name: Option<String>,
    /// Slurm's account is the closest analogue of a PBS project.
    pub project: Option<String>,
}

/// Slurm job identity from the environment.
pub fn slurm_identity_from<F>(get: F) -> JobIdentity
where
    F: Fn(&str) -> Option<String>,
{
    let s = |key: &str| -> Option<String> {
        let v = get(key)?.trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    };
    JobIdentity {
        job_id: s("SLURM_JOB_ID").or_else(|| s("SLURM_JOBID")),
        queue: s("SLURM_JOB_PARTITION"),
        job_name: s("SLURM_JOB_NAME"),
        project: s("SLURM_JOB_ACCOUNT"),
    }
}

/// Parse a Slurm time specification into seconds.
///
/// Slurm accepts six shapes and `SLURM_TIMELIMIT` can carry any of them
/// depending on how the job was submitted:
///
/// ```text
///   minutes                     "60"
///   minutes:seconds             "60:30"
///   hours:minutes:seconds       "01:00:30"
///   days-hours                  "2-12"
///   days-hours:minutes          "2-12:30"
///   days-hours:minutes:seconds  "2-12:30:15"
/// ```
///
/// The bare-number case is the trap: `"60"` means sixty *minutes*, not sixty
/// seconds. Reading it as seconds would report a one-hour booking as one
/// minute and make every walltime-efficiency figure wrong by 60x — the kind of
/// plausible-looking number that is worse than no number at all.
///
/// `UNLIMITED` and anything unparseable yield `None` rather than a guess.
pub fn parse_slurm_time(spec: &str) -> Option<u64> {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("unlimited") {
        return None;
    }

    // `had_day`, not `days > 0`. What decides whether the leading field is
    // hours or minutes is the *presence of the separator*, not the value
    // before it: "0-12" is twelve hours, while "12" is twelve minutes. Keying
    // off the value would collapse those two onto the same answer whenever the
    // day count happened to be zero.
    let (days, rest, had_day) = match spec.split_once('-') {
        Some((d, r)) => (d.trim().parse::<u64>().ok()?, r.trim(), true),
        None => (0, spec, false),
    };

    let parts: Vec<&str> = rest.split(':').collect();
    let nums: Option<Vec<u64>> = parts.iter().map(|p| p.trim().parse::<u64>().ok()).collect();
    let nums = nums?;

    let (h, m, s) = match (had_day, nums.as_slice()) {
        // With a day component the leading field is always hours.
        (true, [h]) => (*h, 0, 0),
        (true, [h, m]) => (*h, *m, 0),
        (true, [h, m, s]) => (*h, *m, *s),
        // Without one, a lone number is minutes and a pair is minutes:seconds.
        (false, [m]) => (0, *m, 0),
        (false, [m, s]) => (0, *m, *s),
        (false, [h, m, s]) => (*h, *m, *s),
        _ => return None,
    };

    Some(days * 86_400 + h * 3_600 + m * 60 + s)
}

/// Booked walltime from Slurm's environment, as `HH:MM:SS`.
///
/// `SLURM_TIMELIMIT` is not exported by every site — where it is missing the
/// shell library asks `scontrol` instead, the same shape as the `qstat`
/// fallback PBS needs.
pub fn slurm_walltime_from<F>(get: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let secs = parse_slurm_time(&get("SLURM_TIMELIMIT")?)?;
    Some(format_hms_secs(secs))
}

/// Booked memory from Slurm's environment.
///
/// Slurm expresses this two mutually exclusive ways, both in **megabytes**:
/// `SLURM_MEM_PER_NODE` for `--mem`, or `SLURM_MEM_PER_CPU` for `--mem-per-cpu`,
/// which has to be multiplied by the CPUs actually on this node.
///
/// Returned with an explicit `MB` suffix so it parses the same way a PBS
/// `-l mem=` string does downstream. A bare number would be ambiguous, and the
/// dashboard would have to guess the unit.
pub fn slurm_booked_mem_from<F>(get: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let num = |key: &str| -> Option<u64> { get(key)?.trim().parse::<u64>().ok() };

    if let Some(per_node) = num("SLURM_MEM_PER_NODE") {
        if per_node > 0 {
            return Some(format!("{per_node}MB"));
        }
    }
    // Per-CPU only becomes a per-node figure once multiplied by this node's CPU
    // count. Without that count the value is not usable, so report nothing
    // rather than a per-CPU number labelled as the node's booking.
    let per_cpu = num("SLURM_MEM_PER_CPU")?;
    let cpus = num("SLURM_CPUS_ON_NODE").or_else(|| num("SLURM_JOB_CPUS_PER_NODE"))?;
    if per_cpu == 0 || cpus == 0 {
        return None;
    }
    Some(format!("{}MB", per_cpu.saturating_mul(cpus)))
}

fn format_hms_secs(total: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn test_empty_environment() {
        let meta = scheduler_metadata_from(env_from(&[]));
        assert_eq!(meta, SchedulerMetadata::default());
        assert!(meta.num_nodes.is_none());
        assert!(meta.tasks_per_node.is_none());
        assert!(meta.cpus_per_task.is_none());
    }

    #[test]
    fn test_pbs_reports_node_count_only() {
        let meta =
            scheduler_metadata_from(env_from(&[("PBS_NUM_NODES", "4"), ("PBS_NCPUS", "16")]));
        assert_eq!(meta.num_nodes, Some(4));
        // PBS has no concept of tasks, and NCPUS is per chunk rather than for
        // the whole allocation, so nothing is inferred here.
        assert_eq!(meta.cpus_per_task, None);
        assert_eq!(meta.tasks_per_node, None);
    }

    #[test]
    fn test_pbs_does_not_divide_per_node_ncpus() {
        // Observed on Gadi: a 96-CPU job across 2 nodes has NCPUS=48, which is
        // the per-node figure. Dividing it reported 24 CPUs for a 48-CPU node.
        let meta = scheduler_metadata_from(env_from(&[("PBS_NNODES", "2"), ("NCPUS", "48")]));
        assert_eq!(meta.num_nodes, Some(2));
        assert_ne!(
            meta.cpus_per_task,
            Some(24),
            "must not halve a per-node NCPUS"
        );
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_pbs_single_node() {
        let meta = scheduler_metadata_from(env_from(&[("NCPUS", "48")]));
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_slurm_overrides_pbs() {
        let meta = scheduler_metadata_from(env_from(&[
            ("PBS_NUM_NODES", "4"),
            ("PBS_NCPUS", "16"),
            ("SLURM_JOB_NUM_NODES", "8"),
            ("SLURM_NTASKS_PER_NODE", "2"),
            ("SLURM_CPUS_PER_TASK", "4"),
        ]));
        assert_eq!(meta.num_nodes, Some(8));
        assert_eq!(meta.tasks_per_node, Some(2));
        assert_eq!(meta.cpus_per_task, Some(4));
    }

    #[test]
    fn test_ignores_unparseable_values() {
        let meta = scheduler_metadata_from(env_from(&[
            ("PBS_NUM_NODES", "not-a-number"),
            ("PBS_NCPUS", ""),
        ]));
        assert_eq!(meta.num_nodes, None);
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_zero_node_count_is_harmless() {
        let meta =
            scheduler_metadata_from(env_from(&[("PBS_NUM_NODES", "0"), ("PBS_NCPUS", "16")]));
        assert_eq!(meta.cpus_per_task, None);
    }

    #[test]
    fn test_reading_real_environment_does_not_panic() {
        let _ = get_scheduler_metadata();
    }

    // ---------------------------------------------------------------- Slurm

    #[test]
    fn test_slurm_time_bare_number_is_minutes() {
        // The one that matters. Slurm's `--time=60` is sixty minutes; reading
        // it as seconds understates the booking by 60x and every
        // walltime-efficiency figure downstream inherits that.
        assert_eq!(parse_slurm_time("60"), Some(3_600));
        assert_eq!(parse_slurm_time("1"), Some(60));
    }

    #[test]
    fn test_slurm_time_all_six_formats() {
        assert_eq!(parse_slurm_time("30"), Some(1_800)); // minutes
        assert_eq!(parse_slurm_time("60:30"), Some(3_630)); // mm:ss
        assert_eq!(parse_slurm_time("01:00:30"), Some(3_630)); // hh:mm:ss
        assert_eq!(parse_slurm_time("2-12"), Some(216_000)); // dd-hh
        assert_eq!(parse_slurm_time("2-12:30"), Some(217_800)); // dd-hh:mm
        assert_eq!(parse_slurm_time("2-12:30:15"), Some(217_815)); // dd-hh:mm:ss
    }

    #[test]
    fn test_slurm_time_day_component_changes_the_leading_field() {
        // "12" alone is 12 minutes; "0-12" is 12 hours. Same digits, different
        // meaning, decided entirely by the day separator.
        assert_eq!(parse_slurm_time("12"), Some(720));
        assert_eq!(parse_slurm_time("0-12"), Some(43_200));
    }

    #[test]
    fn test_slurm_time_rejects_rather_than_guesses() {
        for bad in [
            "",
            "   ",
            "UNLIMITED",
            "unlimited",
            "abc",
            "1:2:3:4",
            "-",
            "2-",
            "1:x",
        ] {
            assert_eq!(parse_slurm_time(bad), None, "expected None for {bad:?}");
        }
    }

    #[test]
    fn test_slurm_walltime_formats_as_hms() {
        let env = |k: &str| match k {
            "SLURM_TIMELIMIT" => Some("90".to_string()),
            _ => None,
        };
        assert_eq!(slurm_walltime_from(env), Some("01:30:00".to_string()));
    }

    #[test]
    fn test_slurm_walltime_absent_when_unset() {
        assert_eq!(slurm_walltime_from(|_: &str| None), None);
    }

    #[test]
    fn test_slurm_mem_per_node_wins() {
        let env = |k: &str| match k {
            "SLURM_MEM_PER_NODE" => Some("64000".to_string()),
            "SLURM_MEM_PER_CPU" => Some("2000".to_string()),
            "SLURM_CPUS_ON_NODE" => Some("8".to_string()),
            _ => None,
        };
        // --mem and --mem-per-cpu are mutually exclusive at submission; if both
        // somehow appear, the node-level figure is the one that describes the
        // booking directly.
        assert_eq!(slurm_booked_mem_from(env), Some("64000MB".to_string()));
    }

    #[test]
    fn test_slurm_mem_per_cpu_multiplied_by_node_cpus() {
        let env = |k: &str| match k {
            "SLURM_MEM_PER_CPU" => Some("2000".to_string()),
            "SLURM_CPUS_ON_NODE" => Some("64".to_string()),
            _ => None,
        };
        assert_eq!(slurm_booked_mem_from(env), Some("128000MB".to_string()));
    }

    #[test]
    fn test_slurm_mem_per_cpu_without_cpu_count_reports_nothing() {
        // A per-CPU number labelled as the node's booking would be wrong by the
        // core count. Better to have no figure than a confidently wrong one.
        let env = |k: &str| match k {
            "SLURM_MEM_PER_CPU" => Some("2000".to_string()),
            _ => None,
        };
        assert_eq!(slurm_booked_mem_from(env), None);
    }

    #[test]
    fn test_slurm_identity() {
        let env = |k: &str| match k {
            "SLURM_JOB_ID" => Some("12345".to_string()),
            "SLURM_JOB_PARTITION" => Some("work".to_string()),
            "SLURM_JOB_NAME" => Some("my_model".to_string()),
            "SLURM_JOB_ACCOUNT" => Some("pawsey0001".to_string()),
            _ => None,
        };
        let id = slurm_identity_from(env);
        assert_eq!(id.job_id.as_deref(), Some("12345"));
        assert_eq!(id.queue.as_deref(), Some("work"));
        assert_eq!(id.job_name.as_deref(), Some("my_model"));
        assert_eq!(id.project.as_deref(), Some("pawsey0001"));
    }

    #[test]
    fn test_slurm_identity_falls_back_to_legacy_jobid() {
        let env = |k: &str| match k {
            "SLURM_JOBID" => Some("999".to_string()),
            _ => None,
        };
        assert_eq!(slurm_identity_from(env).job_id.as_deref(), Some("999"));
    }

    #[test]
    fn test_slurm_identity_blank_is_absent_not_empty() {
        // An exported-but-empty variable must not shadow a better source.
        let env = |k: &str| match k {
            "SLURM_JOB_PARTITION" => Some("   ".to_string()),
            _ => None,
        };
        assert_eq!(slurm_identity_from(env).queue, None);
    }
}
