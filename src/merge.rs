//! Combine per-node summaries into one job-level summary.
//!
//! A multi-node job runs one logger per node, each writing its own NDJSON and
//! its own summary to the shared filesystem. This module reads those summaries
//! back and produces a single job-wide view.
//!
//! Merging happens after the fact, from files, rather than over the network
//! while the job runs. That keeps the pull-based architecture intact: no ports
//! between compute nodes, no collector to stall, and a node whose logger died
//! degrades the result rather than breaking it.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::types::{MergedNotes, MergedSummary, NodeSummary, TelemetrySummary};

/// Find the per-node summary files for `job_id` in `dir`.
///
/// Matches the layout the wrapper writes: `psutil_<jobid>_<host>.log.summary.json`.
/// The merged output is itself named `psutil_<jobid>.summary.json`, so it is
/// skipped explicitly — re-merging a directory must not fold a previous merge
/// back into the totals.
pub fn find_node_summaries(dir: &Path, job_id: &str) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read merge directory {:?}", dir))?;

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if !name.ends_with(".summary.json") {
            continue;
        }
        if !name.contains(job_id) {
            continue;
        }
        if is_merged_summary_name(name, job_id) {
            continue;
        }
        found.push(path);
    }

    // Deterministic order so the per_node array is stable between runs.
    found.sort();
    Ok(found)
}

/// Find the NDJSON stream files for `job_id` in `dir`.
///
/// Used only for diagnostics. When a merge finds no summaries, the useful
/// question is whether the loggers ran at all, and the streams answer it: they
/// are flushed every couple of seconds throughout the run, so their presence
/// means measurement happened and only the end-of-run write was lost. That is
/// the signature of a job killed at its walltime limit, and it points at a
/// different remedy than "the loggers never started".
pub fn find_node_logs(dir: &Path, job_id: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.ends_with(".log") && name.contains(job_id) {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// True for the merged output's own filename, `psutil_<jobid>.summary.json`.
fn is_merged_summary_name(name: &str, job_id: &str) -> bool {
    name == merged_summary_filename(job_id)
}

/// Filename for a job's merged summary.
pub fn merged_summary_filename(job_id: &str) -> String {
    format!(
        "psutil_{}.summary.json",
        crate::host::sanitise_for_filename(job_id)
    )
}

/// Read and parse the per-node summaries, skipping any that are unreadable.
///
/// A single corrupt or half-written file must not sink the merge — the whole
/// point is to salvage a job-level view from whatever the nodes managed to
/// write. Failures are reported to stderr and counted.
pub fn load_node_summaries(paths: &[PathBuf]) -> (Vec<TelemetrySummary>, Vec<String>) {
    let mut summaries = Vec::new();
    let mut failures = Vec::new();

    for path in paths {
        match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|text| {
                serde_json::from_str::<TelemetrySummary>(&text).map_err(|e| e.to_string())
            }) {
            Ok(summary) => summaries.push(summary),
            Err(e) => {
                eprintln!("WARNING: skipping unreadable summary {:?}: {}", path, e);
                failures.push(path.display().to_string());
            }
        }
    }

    (summaries, failures)
}

/// Combine per-node summaries into a job-level view.
///
/// `expected_nodes` is the allocation as the scheduler described it, used to
/// spot nodes that never reported.
pub fn merge_summaries(
    summaries: &[TelemetrySummary],
    expected_nodes: &[String],
) -> Result<MergedSummary> {
    if summaries.is_empty() {
        anyhow::bail!("no per-node summaries to merge");
    }

    // Job-level identity comes from the first node; they should all agree.
    let first = &summaries[0];

    let mut per_node: Vec<NodeSummary> = Vec::with_capacity(summaries.len());

    let mut cpu_core_seconds = 0.0f64;
    let mut total_cpus = 0usize;
    let mut samples_total = 0usize;
    let mut io_read = 0u64;
    let mut io_write = 0u64;
    let mut net_recv = 0u64;
    let mut net_sent = 0u64;
    let mut major_faults = 0u64;
    let mut minor_faults = 0u64;

    let mut cpu_peak_max = 0.0f64;
    let mut cpu_peak_sum = 0.0f64;
    let mut mem_peak_max = 0u64;
    let mut mem_peak_sum = 0u64;
    let mut procs_peak_max = 0usize;
    let mut threads_peak_max = 0usize;

    let mut duration_max = 0.0f64;
    let mut t_start = first.t_start.clone();
    let mut t_end = first.t_end.clone();

    let mut reporting: BTreeSet<String> = BTreeSet::new();

    for s in summaries {
        let host = if s.hostname.is_empty() {
            "unknown".to_string()
        } else {
            s.hostname.clone()
        };
        reporting.insert(host.clone());

        let cpus = s.allowed_cpus.len();
        let mem_peak = s.effective_peak_mem_bytes();

        cpu_core_seconds += s.cpu_core_seconds;
        total_cpus += cpus;
        samples_total += s.samples;
        io_read = io_read.saturating_add(s.io_read_bytes_total);
        io_write = io_write.saturating_add(s.io_write_bytes_total);
        net_recv = net_recv.saturating_add(s.net_recv_bytes_total);
        net_sent = net_sent.saturating_add(s.net_sent_bytes_total);
        major_faults = major_faults.saturating_add(s.major_faults_total);
        minor_faults = minor_faults.saturating_add(s.minor_faults_total);

        cpu_peak_max = cpu_peak_max.max(s.cpu_pct_sum_peak);
        cpu_peak_sum += s.cpu_pct_sum_peak;
        mem_peak_max = mem_peak_max.max(mem_peak);
        mem_peak_sum = mem_peak_sum.saturating_add(mem_peak);
        procs_peak_max = procs_peak_max.max(s.n_procs_peak);
        threads_peak_max = threads_peak_max.max(s.n_threads_peak);

        duration_max = duration_max.max(s.duration_sec);

        // ISO 8601 with a fixed offset sorts lexicographically within one
        // offset, which is all we need: every node of a job shares a timezone.
        if !s.t_start.is_empty() && (t_start.is_empty() || s.t_start < t_start) {
            t_start = s.t_start.clone();
        }
        if !s.t_end.is_empty() && (t_end.is_empty() || s.t_end > t_end) {
            t_end = s.t_end.clone();
        }

        per_node.push(NodeSummary {
            hostname: host,
            node_rank: s.node_rank,
            duration_sec: s.duration_sec,
            samples: s.samples,
            cpus,
            cpu_core_seconds: s.cpu_core_seconds,
            cpu_pct_sum_peak: s.cpu_pct_sum_peak,
            cpu_efficiency_pct_avg: s.cpu_efficiency_pct_avg,
            mem_peak_bytes: mem_peak,
            exit_status: s.exit_status,
            partial: s.partial,
        });
    }

    per_node.sort_by(|a, b| {
        a.node_rank
            .cmp(&b.node_rank)
            .then_with(|| a.hostname.cmp(&b.hostname))
    });

    let nodes_missing: Vec<String> = expected_nodes
        .iter()
        .filter(|n| !reporting.contains(*n))
        .cloned()
        .collect();

    // Taken from per_node rather than from `summaries` so the list is in the
    // same sorted order as the per-node array a reader is looking at.
    let nodes_partial: Vec<String> = per_node
        .iter()
        .filter(|n| n.partial)
        .map(|n| n.hostname.clone())
        .collect();

    // A ratio of two sums, so this is exact even though the peaks are not.
    let cpu_efficiency_pct_avg = if duration_max > 0.0 && total_cpus > 0 {
        (cpu_core_seconds / (duration_max * total_cpus as f64)) * 100.0
    } else {
        0.0
    };

    let walltime_efficiency_pct = if first.booked_walltime_sec > 0 {
        (duration_max / first.booked_walltime_sec as f64) * 100.0
    } else {
        0.0
    };

    let (exit_status, exit_reason) = merge_exit_status(summaries);

    Ok(MergedSummary {
        event: "job_summary".to_string(),
        schema: "merged-v1".to_string(),
        user_id: first.user_id.clone(),
        job_id: first.job_id.clone(),
        queue: first.queue.clone(),
        job_name: first.job_name.clone(),
        project: first.project.clone(),
        t_start,
        t_end,
        duration_sec: duration_max,
        num_nodes_reporting: reporting.len(),
        num_nodes_allocated: first
            .num_nodes_allocated
            .or_else(|| u32::try_from(expected_nodes.len()).ok().filter(|n| *n > 0)),
        nodes_missing,
        nodes_partial,
        total_cpus,
        samples_total,
        cpu_core_seconds,
        cpu_core_hours: cpu_core_seconds / 3600.0,
        io_read_bytes_total: io_read,
        io_write_bytes_total: io_write,
        net_recv_bytes_total: net_recv,
        net_sent_bytes_total: net_sent,
        major_faults_total: major_faults,
        minor_faults_total: minor_faults,
        cpu_pct_sum_peak_max_node: cpu_peak_max,
        cpu_pct_sum_peak_sum_of_nodes: cpu_peak_sum,
        mem_peak_bytes_max_node: mem_peak_max,
        mem_peak_bytes_sum_of_nodes: mem_peak_sum,
        n_procs_peak_max_node: procs_peak_max,
        n_threads_peak_max_node: threads_peak_max,
        cpu_efficiency_pct_avg,
        walltime_efficiency_pct,
        booked_walltime: first.booked_walltime.clone(),
        booked_walltime_sec: first.booked_walltime_sec,
        exit_status,
        exit_reason,
        per_node,
        notes: MergedNotes::default(),
    })
}

/// Worst outcome across nodes.
///
/// A job is only a success if *every* node succeeded, so any non-zero status
/// wins. An unknown status anywhere means the job outcome is unknown — it
/// cannot be reported as success, because the node we have no word from is
/// exactly the one that may have died.
fn merge_exit_status(summaries: &[TelemetrySummary]) -> (Option<i32>, String) {
    let mut worst: Option<i32> = None;
    let mut any_unknown = false;
    let mut unknown_hosts = Vec::new();

    for s in summaries {
        match s.exit_status {
            Some(0) => {}
            Some(code) => {
                // Keep the first non-zero we see; they are all equally "failed".
                if worst.unwrap_or(0) == 0 {
                    worst = Some(code);
                }
            }
            None => {
                any_unknown = true;
                if !s.hostname.is_empty() {
                    unknown_hosts.push(s.hostname.clone());
                }
            }
        }
    }

    if let Some(code) = worst {
        return (Some(code), crate::logger::describe_exit_code(code));
    }
    if any_unknown {
        let hosts = if unknown_hosts.is_empty() {
            String::new()
        } else {
            format!(" ({})", unknown_hosts.join(", "))
        };
        return (
            None,
            format!("unknown: no exit status recorded on some nodes{hosts}"),
        );
    }

    (Some(0), "completed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(
        hostname: &str,
        rank: u32,
        cpus: usize,
        core_secs: f64,
        peak_pct: f64,
    ) -> TelemetrySummary {
        let mut s = TelemetrySummary::new(
            "sam".into(),
            "12345.gadi-pbs".into(),
            "normal".into(),
            "my_model".into(),
            "ab12".into(),
            1,
            0.5,
            (0..cpus as u32).collect(),
            "cgroup-v1".into(),
            "/x".into(),
            None,
            "02:00:00".into(),
            7200,
            "16GB".into(),
            17_179_869_184,
        );
        s.hostname = hostname.to_string();
        s.node_rank = Some(rank);
        s.duration_sec = 100.0;
        s.samples = 200;
        s.cpu_core_seconds = core_secs;
        s.cpu_pct_sum_peak = peak_pct;
        s.rss_bytes_peak = 1_000_000_000;
        s.exit_status = Some(0);
        s.t_start = "2026-07-27T10:00:00+1000".into();
        s.t_end = "2026-07-27T10:01:40+1000".into();
        s
    }

    #[test]
    fn test_merge_sums_core_seconds_exactly() {
        let nodes = vec![
            node("gadi-cpu-clx-0001", 0, 48, 4000.0, 4500.0),
            node("gadi-cpu-clx-0002", 1, 48, 3500.0, 4700.0),
        ];
        let merged = merge_summaries(&nodes, &[]).unwrap();

        assert_eq!(merged.num_nodes_reporting, 2);
        assert_eq!(merged.total_cpus, 96);
        assert!((merged.cpu_core_seconds - 7500.0).abs() < 1e-9);
        assert!((merged.cpu_core_hours - 7500.0 / 3600.0).abs() < 1e-9);
        assert_eq!(merged.samples_total, 400);
    }

    #[test]
    fn test_merge_reports_both_peak_bounds() {
        let nodes = vec![
            node("a", 0, 48, 4000.0, 4500.0),
            node("b", 1, 48, 3500.0, 4700.0),
        ];
        let merged = merge_summaries(&nodes, &[]).unwrap();

        // Some node definitely hit 4700.
        assert!((merged.cpu_pct_sum_peak_max_node - 4700.0).abs() < 1e-9);
        // The job cannot have exceeded the sum, but probably never reached it.
        assert!((merged.cpu_pct_sum_peak_sum_of_nodes - 9200.0).abs() < 1e-9);
        assert!(merged.cpu_pct_sum_peak_sum_of_nodes > merged.cpu_pct_sum_peak_max_node);
    }

    #[test]
    fn test_merge_efficiency_is_ratio_of_sums() {
        // 2 nodes x 48 cpus x 100s = 9600 core-seconds available; 4800 used.
        let mut a = node("a", 0, 48, 2400.0, 0.0);
        let mut b = node("b", 1, 48, 2400.0, 0.0);
        a.duration_sec = 100.0;
        b.duration_sec = 100.0;

        let merged = merge_summaries(&[a, b], &[]).unwrap();
        assert!(
            (merged.cpu_efficiency_pct_avg - 50.0).abs() < 1e-9,
            "got {}",
            merged.cpu_efficiency_pct_avg
        );
    }

    #[test]
    fn test_merge_uses_longest_node_as_job_duration() {
        let mut a = node("a", 0, 4, 10.0, 0.0);
        let mut b = node("b", 1, 4, 10.0, 0.0);
        a.duration_sec = 100.0;
        b.duration_sec = 250.0;

        let merged = merge_summaries(&[a, b], &[]).unwrap();
        assert!((merged.duration_sec - 250.0).abs() < 1e-9);
    }

    #[test]
    fn test_merge_spans_earliest_start_to_latest_end() {
        let mut a = node("a", 0, 4, 1.0, 0.0);
        let mut b = node("b", 1, 4, 1.0, 0.0);
        a.t_start = "2026-07-27T10:00:05+1000".into();
        a.t_end = "2026-07-27T10:05:00+1000".into();
        b.t_start = "2026-07-27T10:00:01+1000".into();
        b.t_end = "2026-07-27T10:04:00+1000".into();

        let merged = merge_summaries(&[a, b], &[]).unwrap();
        assert_eq!(merged.t_start, "2026-07-27T10:00:01+1000");
        assert_eq!(merged.t_end, "2026-07-27T10:05:00+1000");
    }

    #[test]
    fn test_merge_flags_nodes_that_never_reported() {
        let nodes = vec![node("a", 0, 4, 1.0, 0.0), node("b", 1, 4, 1.0, 0.0)];
        let expected = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let merged = merge_summaries(&nodes, &expected).unwrap();

        // Silently reporting 2/3 of the usage as if it were the whole job would
        // be the worst possible outcome here.
        assert_eq!(merged.nodes_missing, vec!["c".to_string()]);
        assert_eq!(merged.num_nodes_reporting, 2);
        assert_eq!(merged.num_nodes_allocated, Some(3));
    }

    #[test]
    fn test_merge_flags_nodes_that_only_checkpointed() {
        let mut a = node("a", 0, 4, 1.0, 0.0);
        let mut b = node("b", 1, 4, 1.0, 0.0);
        let c = node("c", 2, 4, 1.0, 0.0);
        a.partial = true;
        b.partial = true;

        let expected = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let merged = merge_summaries(&[a, b, c], &expected).unwrap();

        // A partial node is not a missing one: it reported, just not to the end.
        // Conflating them would either hide the shortfall or double-count it.
        assert_eq!(merged.nodes_partial, vec!["a".to_string(), "b".to_string()]);
        assert!(merged.nodes_missing.is_empty());
        assert_eq!(merged.num_nodes_reporting, 3);
        assert_eq!(merged.per_node.iter().filter(|n| n.partial).count(), 2);
    }

    #[test]
    fn test_merge_of_clean_nodes_reports_nothing_partial() {
        // The control for the test above: without the flag set, no node is
        // labelled, so a normal job's summary is unchanged by this feature.
        let nodes = vec![node("a", 0, 4, 1.0, 0.0), node("b", 1, 4, 1.0, 0.0)];
        let expected = vec!["a".to_string(), "b".to_string()];

        let merged = merge_summaries(&nodes, &expected).unwrap();

        assert!(merged.nodes_partial.is_empty());
        assert!(merged.per_node.iter().all(|n| !n.partial));
    }

    #[test]
    fn test_merge_exit_status_any_failure_wins() {
        let mut a = node("a", 0, 4, 1.0, 0.0);
        let mut b = node("b", 1, 4, 1.0, 0.0);
        a.exit_status = Some(0);
        b.exit_status = Some(137);

        let merged = merge_summaries(&[a, b], &[]).unwrap();
        assert_eq!(merged.exit_status, Some(137));
        assert!(merged.exit_reason.contains("SIGKILL"));
    }

    #[test]
    fn test_merge_exit_status_unknown_is_not_success() {
        let mut a = node("a", 0, 4, 1.0, 0.0);
        let mut b = node("b", 1, 4, 1.0, 0.0);
        a.exit_status = Some(0);
        b.exit_status = None;

        let merged = merge_summaries(&[a, b], &[]).unwrap();
        assert_eq!(
            merged.exit_status, None,
            "a node we have no word from may be the one that died"
        );
        assert!(merged.exit_reason.contains("unknown"));
        assert!(merged.exit_reason.contains('b'));
    }

    #[test]
    fn test_merge_all_success() {
        let merged =
            merge_summaries(&[node("a", 0, 4, 1.0, 0.0), node("b", 1, 4, 1.0, 0.0)], &[]).unwrap();
        assert_eq!(merged.exit_status, Some(0));
        assert_eq!(merged.exit_reason, "completed");
    }

    #[test]
    fn test_merge_single_node_still_works() {
        let merged = merge_summaries(&[node("solo", 0, 48, 4000.0, 4500.0)], &[]).unwrap();
        assert_eq!(merged.num_nodes_reporting, 1);
        assert_eq!(merged.total_cpus, 48);
        assert!(
            (merged.cpu_pct_sum_peak_max_node - merged.cpu_pct_sum_peak_sum_of_nodes).abs() < 1e-9
        );
    }

    #[test]
    fn test_merge_empty_is_an_error() {
        assert!(merge_summaries(&[], &[]).is_err());
    }

    #[test]
    fn test_per_node_is_ordered_by_rank() {
        let nodes = vec![
            node("zeta", 2, 4, 1.0, 0.0),
            node("alpha", 0, 4, 1.0, 0.0),
            node("mid", 1, 4, 1.0, 0.0),
        ];
        let merged = merge_summaries(&nodes, &[]).unwrap();
        let ranks: Vec<_> = merged.per_node.iter().map(|n| n.node_rank).collect();
        assert_eq!(ranks, vec![Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn test_merged_filename_and_skip() {
        assert_eq!(
            merged_summary_filename("12345.gadi-pbs"),
            "psutil_12345.gadi-pbs.summary.json"
        );
        // Re-merging a directory must not fold a previous merge into the totals.
        assert!(is_merged_summary_name(
            "psutil_12345.gadi-pbs.summary.json",
            "12345.gadi-pbs"
        ));
        assert!(!is_merged_summary_name(
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0001.log.summary.json",
            "12345.gadi-pbs"
        ));
    }

    #[test]
    fn test_find_node_summaries_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let job = "12345.gadi-pbs";

        for name in [
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0002.log.summary.json",
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0001.log.summary.json",
            "psutil_12345.gadi-pbs.summary.json", // previous merge — must be skipped
            "psutil_99999.other-job_host.log.summary.json", // different job
            "psutil_12345.gadi-pbs_gadi-cpu-clx-0001.log", // not a summary
        ] {
            std::fs::write(p.join(name), "{}").unwrap();
        }

        let found = find_node_summaries(p, job).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(
            names,
            vec![
                "psutil_12345.gadi-pbs_gadi-cpu-clx-0001.log.summary.json",
                "psutil_12345.gadi-pbs_gadi-cpu-clx-0002.log.summary.json",
            ]
        );
    }

    #[test]
    fn test_load_skips_corrupt_files_without_failing() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.summary.json");
        let bad = dir.path().join("bad.summary.json");

        std::fs::write(
            &good,
            serde_json::to_string(&node("a", 0, 4, 1.0, 0.0)).unwrap(),
        )
        .unwrap();
        // Half-written, as if the node died mid-flush.
        std::fs::write(&bad, "{\"user_id\": \"sam\", \"job_i").unwrap();

        let (summaries, failures) = load_node_summaries(&[good, bad]);

        assert_eq!(summaries.len(), 1, "the good summary must still be usable");
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn test_find_node_logs_distinguishes_killed_from_never_ran() {
        let dir = tempfile::tempdir().unwrap();
        let job = "12345.gadi-pbs";

        // Nothing at all: the loggers never started.
        assert!(find_node_logs(dir.path(), job).is_empty());

        // Streams but no summaries: the loggers ran and were killed before
        // their final write. These two cases need different advice, which is
        // the whole reason this function exists.
        std::fs::write(dir.path().join(format!("psutil_{job}_node1.log")), "{}\n").unwrap();
        std::fs::write(dir.path().join(format!("psutil_{job}_node2.log")), "{}\n").unwrap();
        // Another job's stream must not be counted.
        std::fs::write(dir.path().join("psutil_99999.gadi-pbs_node1.log"), "{}\n").unwrap();
        // Nor the summary files, which do not end in `.log`.
        std::fs::write(
            dir.path()
                .join(format!("psutil_{job}_node1.log.summary.json")),
            "{}",
        )
        .unwrap();

        let logs = find_node_logs(dir.path(), job);
        assert_eq!(logs.len(), 2, "only this job's NDJSON streams: {logs:?}");
        assert!(logs.iter().all(|p| p.to_str().unwrap().contains(job)));
    }
}
