use anyhow::{Context, Result};
use clap::Parser;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use hpc_telemetry::cli::Args;
use hpc_telemetry::logger::TelemetryLogger;
use hpc_telemetry::manifest;
use hpc_telemetry::merge;
use hpc_telemetry::output::write_merged_summary;

fn main() -> Result<()> {
    let mut args = Args::parse();
    args.resolve_and_validate()?;

    // First, because it is the one mode that must work on a half-configured
    // system — that is what it is for.
    if args.is_check_mode() {
        std::process::exit(hpc_telemetry::check::run(&args));
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
fn run_merge(args: &Args) -> Result<()> {
    let dir = args
        .merge
        .as_ref()
        .expect("merge mode implies --merge is set");
    let job_id = args.get_job_id();

    let paths = merge::find_node_summaries(dir, &job_id)?;
    if paths.is_empty() {
        anyhow::bail!(
            "no per-node summaries for job {} found in {:?}. Each node's logger writes \
             <output>.summary.json when it stops; if none exist, the loggers did not run \
             or were killed before they could finish.",
            job_id,
            dir
        );
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
    if !failures.is_empty() {
        eprintln!(
            "WARNING: {} summary file(s) were unreadable and were skipped.",
            failures.len()
        );
    }

    Ok(())
}
