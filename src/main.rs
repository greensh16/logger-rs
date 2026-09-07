use anyhow::{Context, Result};
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use logger_rs::cli::Args;
use logger_rs::logger::TelemetryLogger;
use logger_rs::manifest;
use logger_rs::merge;
use logger_rs::output::write_merged_summary;

fn main() -> Result<()> {
    let mut args = Args::parse();
    args.resolve_and_validate()?;

    // First, because it is the one mode that must work on a half-configured
    // system — that is what it is for.
    if args.is_check_mode() {
        std::process::exit(logger_rs::check::run(&args));
    }

    if args.is_manifest_mode() {
        return run_write_manifest(&args);
    }

    if args.is_merge_mode() {
        return run_merge(&args);
    }

    let running = Arc::new(AtomicBool::new(true));
    let handler_flag = running.clone();

    // Requires the ctrlc "termination" feature, which adds SIGTERM and SIGHUP
    // to the default SIGINT. PBS sends SIGTERM when a job exhausts its walltime
    // and when it is qdel'd; without this the logger was killed outright,
    // losing the job_end line, the summary, and any buffered samples.
    ctrlc::set_handler(move || {
        // A second signal means someone is impatient — stop immediately rather
        // than waiting for the current tick to finish.
        if !handler_flag.swap(false, Ordering::SeqCst) {
            eprintln!("Second signal received, exiting immediately.");
            std::process::exit(130);
        }
        eprintln!("Signal received, shutting down after the current sample...");
    })
    .context("Failed to install signal handler")?;

    let mut logger = TelemetryLogger::new(args)?;
    logger.run(running)?;

    Ok(())
}

/// Write the manifest naming every file this job will produce.
fn run_write_manifest(args: &Args) -> Result<()> {
    let dir = args
        .write_manifest
        .as_ref()
        .expect("manifest mode implies --write-manifest is set");

    let template = args
        .outfile
        .as_ref()
        .expect("validated above")
        .to_string_lossy()
        .to_string();

    let hosts = args.manifest_hosts();

    let created = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string();

    let manifest = manifest::build_manifest(
        &args.get_job_id(),
        &args.get_user_id(),
        &args.queue,
        &args.job_name,
        &args.project,
        &template,
        &hosts,
        created,
    );

    let path = manifest::write_manifest(dir, &manifest)?;
    eprintln!(
        "Wrote manifest for {} node(s) -> {:?}",
        manifest.nodes_expected, path
    );

    Ok(())
}

/// Combine the per-node summaries of a multi-node job into one job-level file.
/// Explain a merge that found nothing, distinguishing the two causes.
///
/// "No summaries" has two very different meanings and the same message used to
/// cover both. If the NDJSON streams are there, the loggers ran and were killed
/// before their final write — the data is on disk and the fix is on the
/// shutdown path. If nothing is there at all, the loggers never started, and
/// the fix is in the job script.
fn no_summaries_message(dir: &std::path::Path, job_id: &str) -> String {
    let logs = merge::find_node_logs(dir, job_id);

    if logs.is_empty() {
        return format!(
            "no per-node summaries or telemetry logs for job {job_id} found in {dir:?}. \
             The loggers do not appear to have run at all. Check the job's stderr for \
             errors from telemetry_start, and that the output directory is writable \
             from the compute nodes."
        );
    }

    format!(
        "no per-node summaries for job {job_id} found in {dir:?}, but {} telemetry log(s) \
         are there. The loggers ran and were killed before writing their summaries — the \
         usual cause is the job hitting its walltime limit, where the scheduler's SIGKILL \
         arrives before shutdown finishes. The samples themselves are intact in those logs \
         and the dashboard can still read them; only the job-level roll-up is missing. \
         Summary checkpointing (--summary-every, 30s by default) exists to leave \
         something mergeable here, so finding nothing means it was disabled, or the \
         job ended within the first period, or these logs predate it.",
        logs.len()
    )
}

fn run_merge(args: &Args) -> Result<()> {
    let dir = args
        .merge
        .as_ref()
        .expect("merge mode implies --merge is set");
    let job_id = args.get_job_id();

    let paths = merge::find_node_summaries(dir, &job_id)?;
    if paths.is_empty() {
        anyhow::bail!("{}", no_summaries_message(dir, &job_id));
    }

    let (summaries, failures) = merge::load_node_summaries(&paths);
    if summaries.is_empty() {
        anyhow::bail!(
            "found {} summary file(s) for job {} but none could be parsed",
            paths.len(),
            job_id
        );
    }

    let merged = merge::merge_summaries(&summaries, &args.expected_nodes())?;

    let out_path = dir.join(merge::merged_summary_filename(&job_id));
    write_merged_summary(&out_path, &merged)?;

    eprintln!(
        "Merged {} node summaries ({} CPUs, {:.1} core-hours) -> {:?}",
        merged.num_nodes_reporting, merged.total_cpus, merged.cpu_core_hours, out_path
    );
    if !merged.nodes_missing.is_empty() {
        eprintln!(
            "WARNING: {} allocated node(s) never reported: {}. \
             The totals above understate the job.",
            merged.nodes_missing.len(),
            merged.nodes_missing.join(", ")
        );
    }
    if !merged.nodes_partial.is_empty() {
        eprintln!(
            "NOTE: {} node(s) reported a mid-run checkpoint rather than a clean finish: {}. \
             Their figures stop at the last checkpoint, so the totals are a lower bound. \
             This is what a job killed at its walltime limit looks like.",
            merged.nodes_partial.len(),
            merged.nodes_partial.join(", ")
        );
    }
    if !failures.is_empty() {
        eprintln!(
            "WARNING: {} summary file(s) were unreadable and were skipped.",
            failures.len()
        );
    }

    Ok(())
}
