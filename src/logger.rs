//! Main sampling loop.

use anyhow::{Context, Result};
use serde_json::json;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cgroup::{Cgroup, CpuUsage};
use crate::cli::Args;
use crate::cli::ProcsFrom;
use crate::cpu::{calculate_percpu_busy_pct, calculate_total_cpu_pct, cpu_efficiency_pct};
use crate::gpu::GpuSampler;
use crate::network::{read_network_stats, NetworkStats};
use crate::output::{create_ndjson_writer, write_ndjson_line, write_summary_file};
use crate::process::{
    get_process_tree_stats_extended, get_stats_for_pids, ProcSource, ProcessTreeStats, ScanOptions,
};
use crate::scheduler::get_scheduler_metadata;
use crate::types::{
    add_scaled_into, max_into_f64, max_into_u64, TelemetrySample, TelemetrySummary,
};

/// Give up after this many consecutive sampling failures.
///
/// The usual cause is the job's cgroup being torn down, which means the job is
/// over and there is nothing left to measure. Previously the loop retried with
/// no delay and no limit, writing to the PBS stderr file as fast as the CPU
/// allowed.
const MAX_CONSECUTIVE_ERRORS: u32 = 20;

/// How long to back off after a failed sample.
const ERROR_BACKOFF: Duration = Duration::from_millis(500);

/// Longest we let the output buffer sit unflushed, so a SIGKILL loses at most
/// this much data.
const MAX_FLUSH_LAG: f64 = 2.0;

pub struct TelemetryLogger {
    args: Args,
    output_file: BufWriter<File>,
    summary: TelemetrySummary,
    /// Output path after `{host}`/`{jobid}`/`{user}` expansion.
    outfile: PathBuf,
    summary_path: PathBuf,
    samples: usize,
    start_time: Instant,
    /// Guards against `Drop` writing the summary a second time.
    summary_written: bool,
    flush_every: usize,
    /// Ticks between checkpoint summary writes; 0 disables them.
    summary_every: usize,
    /// So a persistently failing checkpoint warns once rather than every period.
    checkpoint_warned: bool,

    /// `None` when no cgroup could be found. Fatal on Linux; on other platforms
    /// the logger runs in stub mode so the binary is still developable.
    cgroup: Option<Cgroup>,
    allowed_cpus: Vec<u32>,
    /// This node's short hostname, stamped on every record so a merged
    /// multi-node view can attribute each reading.
    hostname: String,
    /// Booked memory, resolved once at startup — see [`resolve_booked_mem`].
    booked_mem_bytes: u64,
    booked_mem_display: String,
    /// Where the figure came from, and crucially whether it is per-node.
    booked_mem_source: String,
    booked_mem_is_per_node: bool,

    /// Previous CPU reading and the instant it was taken.
    ///
    /// Carrying this across ticks is what makes the measurement windows
    /// contiguous. Previously each tick read the counter at its own start and
    /// end, so CPU consumed between one tick's end and the next tick's start —
    /// serialising JSON, writing to Lustre, updating the summary — was never
    /// attributed to anything.
    prev_cpu: Option<(CpuUsage, Instant)>,
    prev_net: Option<NetworkStats>,
    gpu: GpuSampler,
    scan_opts: ScanOptions,
    proc_source: ProcSource,
}

impl TelemetryLogger {
    pub fn new(args: Args) -> Result<Self> {
        let hostname = crate::host::hostname();
        let summary_path = args.summary_path();
        let outfile = args.resolved_outfile()?;
        let output_file = create_ndjson_writer(&outfile)?;

        let discovered = match &args.cgroup {
            Some(explicit) => Cgroup::from_explicit_path(explicit, args.tree_pid),
            None => Cgroup::discover(args.tree_pid, &args.get_job_id()),
        };

        let cgroup = match discovered {
            Ok(cg) => {
                eprintln!(
                    "Discovered {} via {} -> {}",
                    cg.version.as_str(),
                    cg.source,
                    cg.rel_path
                );
                if cg.is_root() {
                    eprintln!(
                        "WARNING: this is the ROOT cgroup, so CPU and memory describe the whole \
                         node rather than this job. Pass --cgroup to point at the job's cgroup."
                    );
                }
                eprintln!("   cpu:    {:?}", cg.cpu_path);
                eprintln!("   memory: {:?}", cg.memory_path);
                eprintln!("   io:     {:?}", cg.io_path);
                eprintln!("   cpuset: {:?}", cg.cpuset_path);
                Some(cg)
            }
            Err(e) => {
                // cfg! rather than #[cfg] so both branches type-check on every
                // platform; an attribute-gated block here evaluates to () on the
                // arm that is compiled out.
                if cfg!(target_os = "linux") {
                    return Err(e).context(
                        "cgroup discovery failed. Telemetry cannot be collected without it.",
                    );
                }
                eprintln!("WARNING: no cgroup available ({e}); running in stub mode.");
                eprintln!(
                    "         This is a development build. Deploy to Linux for real metrics."
                );
                None
            }
        };

        let (allowed_cpus, cpuset_source) = match &cgroup {
            Some(cg) => cg.allowed_cpus(),
            None => ((0..num_cpus::get() as u32).collect(), "stub".to_string()),
        };
        eprintln!(
            "   allowed CPUs: {:?} (from {})",
            allowed_cpus, cpuset_source
        );

        // If the cgroup can give us memory and I/O, skip the per-process
        // /proc/{pid}/io and /proc/{pid}/status reads entirely — they are the
        // expensive part of a tree scan and the cgroup's figures are better.
        let cgroup_has_mem = cgroup.as_ref().and_then(|c| c.read_memory()).is_some();
        let cgroup_has_io = cgroup.as_ref().and_then(|c| c.read_io()).is_some();
        let scan_opts = ScanOptions {
            collect_io: !cgroup_has_io,
            collect_swap: !cgroup_has_mem,
        };

        // Which processes count as the job's.
        //
        // The cgroup is authoritative and works everywhere. The tree walk only
        // works where the job's processes are genuinely descendants of
        // --tree-pid, which is true on the head node and false on every other:
        // there the MPI ranks were started by the scheduler, not by us.
        let cgroup_can_list_procs = cgroup
            .as_ref()
            .map(|c| !c.is_root() && c.procs().is_ok())
            .unwrap_or(false);

        let proc_source = match args.procs_from {
            ProcsFrom::Cgroup => {
                if !cgroup_can_list_procs {
                    anyhow::bail!(
                        "--procs-from cgroup was requested but the cgroup cannot list processes \
                         (no cgroup found, it is the root cgroup, or cgroup.procs is empty)"
                    );
                }
                ProcSource::Cgroup
            }
            ProcsFrom::Tree => ProcSource::Tree,
            ProcsFrom::Auto => {
                if cgroup_can_list_procs {
                    ProcSource::Cgroup
                } else {
                    ProcSource::Tree
                }
            }
        };
        eprintln!("   procs:  {}", proc_source.as_str());

        let version_str = cgroup
            .as_ref()
            .map(|c| c.version.as_str().to_string())
            .unwrap_or_else(|| "none".to_string());
        let cpu_path_str = cgroup
            .as_ref()
            .map(|c| c.cpu_path.display().to_string())
            .unwrap_or_else(|| "<stub>".to_string());

        let (booked_mem_bytes, booked_mem_display, booked_mem_source, booked_mem_is_per_node) =
            resolve_booked_mem(&args, cgroup.as_ref());
        eprintln!(
            "   booked mem: {} ({}, {})",
            booked_mem_display,
            booked_mem_source,
            if booked_mem_is_per_node {
                "per node"
            } else {
                "whole job"
            }
        );

        let mut summary = TelemetrySummary::new(
            args.get_user_id(),
            args.get_job_id(),
            args.queue.clone(),
            args.job_name.clone(),
            args.project.clone(),
            args.tree_pid,
            args.interval,
            allowed_cpus.clone(),
            version_str,
            cpu_path_str,
            Some(cpuset_source),
            args.booked_walltime_normalised(),
            args.booked_walltime_seconds(),
            booked_mem_display.clone(),
            booked_mem_bytes,
        );
        summary.booked_mem_source = booked_mem_source.clone();
        summary.booked_mem_is_per_node = booked_mem_is_per_node;

        let sched_meta = get_scheduler_metadata();
        summary.num_nodes = sched_meta.num_nodes.or(Some(1));
        summary.tasks_per_node = sched_meta.tasks_per_node.or(Some(1));
        summary.cpus_per_task = sched_meta
            .cpus_per_task
            .or(Some(allowed_cpus.len().max(1) as u32));
        summary.hostname = hostname.clone();
        summary.node_rank = args.node_rank;
        summary.num_nodes_allocated = sched_meta.num_nodes;
        summary.proc_source = proc_source.as_str().to_string();
        summary.t_start = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%z")
            .to_string();

        // Prime the CPU counter so the very first tick has a proper baseline
        // rather than measuring from zero.
        let prev_cpu = cgroup
            .as_ref()
            .and_then(|c| c.read_cpu_usage().ok())
            .map(|u| (u, Instant::now()));
        let prev_net = read_network_stats().ok();

        let flush_every = ((MAX_FLUSH_LAG / args.interval).round() as usize).max(1);
        // 0 disables checkpointing; anything else becomes at least one tick.
        let summary_every = if args.summary_every <= 0.0 {
            0
        } else {
            ((args.summary_every / args.interval).round() as usize).max(1)
        };
        let gpu = GpuSampler::new(args.gpu_interval);

        Ok(Self {
            args,
            output_file,
            summary,
            outfile,
            summary_path,
            samples: 0,
            start_time: Instant::now(),
            summary_written: false,
            flush_every,
            summary_every,
            checkpoint_warned: false,
            cgroup,
            allowed_cpus,
            hostname,
            booked_mem_bytes,
            booked_mem_display,
            booked_mem_source,
            booked_mem_is_per_node,
            prev_cpu,
            prev_net,
            gpu,
            scan_opts,
            proc_source,
        })
    }

    /// Sample the job's processes, from whichever source was selected.
    fn scan_processes(&self) -> ProcessTreeStats {
        match self.proc_source {
            ProcSource::Cgroup => self
                .cgroup
                .as_ref()
                .and_then(|cg| cg.procs().ok())
                .and_then(|pids| get_stats_for_pids(&pids, self.scan_opts).ok())
                .unwrap_or_default(),
            ProcSource::Tree => get_process_tree_stats_extended(self.args.tree_pid, self.scan_opts)
                .unwrap_or_default(),
        }
    }

    /// Main sampling loop
    pub fn run(&mut self, running: Arc<AtomicBool>) -> Result<()> {
        eprintln!("Starting telemetry logger");
        eprintln!("   PID:      {}", self.args.tree_pid);
        eprintln!("   Interval: {:.2}s", self.args.interval);
        eprintln!("   Node:     {}", self.hostname);
        eprintln!("   Output:   {:?}", self.outfile);
        eprintln!("   Summary:  {:?}", self.summary_path);

        if let Err(e) = self.emit_job_start() {
            eprintln!("WARNING: error writing job_start: {e}");
        }

        let mut consecutive_errors: u32 = 0;
        let mut consecutive_write_errors: u32 = 0;
        let mut ticks_since_stop_check: usize = 0;

        while running.load(Ordering::SeqCst) {
            // A logger on a remote node cannot be signalled reliably: pbsdsh and
            // srun do not consistently forward SIGTERM to the task they
            // launched. Instead every logger watches for the exit-status file
            // the head node writes to the shared filesystem when the workload
            // finishes, which doubles as the shutdown signal and the outcome.
            //
            // Throttled rather than checked every tick: one stat per node per
            // 0.5s is a lot of pointless Lustre metadata traffic on a job
            // spanning hundreds of nodes.
            ticks_since_stop_check += 1;
            if ticks_since_stop_check >= self.flush_every {
                ticks_since_stop_check = 0;
                if self.workload_has_finished() {
                    eprintln!("Workload finished (exit-status file appeared); shutting down.");
                    break;
                }
            }

            if self.cgroup.is_none() {
                // Stub mode: nothing to measure, just idle until told to stop.
                std::thread::sleep(Duration::from_secs_f64(self.args.interval));
                self.samples += 1;
                continue;
            }

            match self.sample_tick() {
                Ok(sample) => {
                    consecutive_errors = 0;

                    // Write failures need the same treatment as sampling
                    // failures. gdata filling up is a routine occurrence, and
                    // unconditionally logging every failure would put one line
                    // per tick into the job's stderr — roughly 350,000 lines
                    // over a 48-hour job at the default interval.
                    match write_ndjson_line(&mut self.output_file, &sample) {
                        Ok(()) => consecutive_write_errors = 0,
                        Err(e) => {
                            consecutive_write_errors += 1;
                            if consecutive_write_errors == 1 {
                                eprintln!("WARNING: error writing sample: {}", e);
                            }
                            if consecutive_write_errors >= MAX_CONSECUTIVE_ERRORS {
                                eprintln!(
                                    "Giving up after {} consecutive write failures to {:?}. \
                                     The output filesystem is most likely full or unreachable.",
                                    MAX_CONSECUTIVE_ERRORS, self.outfile
                                );
                                break;
                            }
                        }
                    }
                    self.update_summary(&sample);
                    self.samples += 1;

                    if self.samples.is_multiple_of(self.flush_every) {
                        let _ = self.output_file.flush();
                    }

                    // Checkpoint the summary so a SIGKILL — which is how a
                    // walltime kill ends — leaves something to merge. The write
                    // goes through the same temp-file-and-rename as the final
                    // one, so a reader never sees a half-written document and
                    // the previous checkpoint stays intact until this one is
                    // complete.
                    if self.summary_every > 0 && self.samples.is_multiple_of(self.summary_every) {
                        if let Err(e) = self.write_checkpoint_summary() {
                            if !self.checkpoint_warned {
                                self.checkpoint_warned = true;
                                eprintln!(
                                    "WARNING: could not checkpoint the summary to {:?}: {e}. \
                                     Sampling continues; further checkpoint failures are silent.",
                                    self.summary_path
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    eprintln!(
                        "WARNING: error sampling ({}/{}): {}",
                        consecutive_errors, MAX_CONSECUTIVE_ERRORS, e
                    );

                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        eprintln!(
                            "Giving up after {} consecutive failures. The job's cgroup has \
                             most likely been torn down.",
                            MAX_CONSECUTIVE_ERRORS
                        );
                        break;
                    }

                    // Back off so a persistent failure cannot spin the CPU and
                    // flood the job's stderr file.
                    std::thread::sleep(ERROR_BACKOFF);
                }
            }
        }

        if let Err(e) = self.emit_job_end() {
            eprintln!("WARNING: error writing job_end: {e}");
        }
        let _ = self.output_file.flush();

        self.write_final_summary()?;

        eprintln!("Telemetry logger stopped. Total samples: {}", self.samples);
        Ok(())
    }

    fn sample_tick(&mut self) -> Result<TelemetrySample> {
        let tick_start = Instant::now();
        let deadline = tick_start + Duration::from_secs_f64(self.args.interval);

        // Sub-sample the process tree across the tick to catch short-lived
        // peaks, keeping the element-wise maximum. Taking the max (rather than
        // the last reading, as before) means a burst of workers that exit
        // before the tick ends still shows up.
        let mut tree = ProcessTreeStats::default();
        loop {
            tree.max_with(&self.scan_processes());

            let now = Instant::now();
            if now >= deadline {
                break;
            }
            // Sleep only as long as is left, so a short interval is not
            // overshot by a whole 100ms.
            let remaining = deadline.saturating_duration_since(now);
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }

        // Scope the cgroup borrow so we can mutate self afterwards.
        let (cpu_now, mem, io) = {
            let cgroup = self
                .cgroup
                .as_ref()
                .context("sample_tick called without a cgroup")?;
            (
                cgroup.read_cpu_usage()?,
                cgroup.read_memory(),
                cgroup.read_io(),
            )
        };
        let read_at = Instant::now();

        let (prev_usage, prev_at) = self
            .prev_cpu
            .take()
            .unwrap_or_else(|| (CpuUsage::default(), tick_start));
        let dt = read_at.duration_since(prev_at).as_secs_f64();

        let percpu_pct = calculate_percpu_busy_pct(&prev_usage.percpu_ns, &cpu_now.percpu_ns, dt);
        let cpu_pct_sum = calculate_total_cpu_pct(prev_usage.total_ns, cpu_now.total_ns, dt);
        self.prev_cpu = Some((cpu_now, read_at));

        // Network deltas, also across contiguous windows.
        let net_now = read_network_stats().unwrap_or_default();
        let net_prev = self.prev_net.take().unwrap_or_default();
        let net_delta = NetworkStats {
            recv_bytes: net_now.recv_bytes.saturating_sub(net_prev.recv_bytes),
            sent_bytes: net_now.sent_bytes.saturating_sub(net_prev.sent_bytes),
            recv_packets: net_now.recv_packets.saturating_sub(net_prev.recv_packets),
            sent_packets: net_now.sent_packets.saturating_sub(net_prev.sent_packets),
        };
        self.prev_net = Some(net_now);

        let gpu_stats = self.gpu.sample();

        // One clock reading, not two. Building the timestamp from separate
        // calls to now() meant a second boundary landing between them produced
        // a timestamp roughly a second in the past — a non-monotonic point in
        // the dashboard's time series.
        let now = chrono::Local::now();
        let t = now.timestamp() as f64 + now.timestamp_subsec_millis() as f64 / 1000.0;

        Ok(TelemetrySample {
            event: "job_log".to_string(),
            user_id: self.args.get_user_id(),
            job_id: self.args.get_job_id(),
            queue: self.args.queue.clone(),
            job_name: self.args.job_name.clone(),
            project: self.args.project.clone(),
            hostname: self.hostname.clone(),
            t,
            dt_sec: dt,
            allowed_cpus: self.allowed_cpus.clone(),
            cpu_pct_sum,
            rss_bytes_sum: tree.rss_bytes,
            n_procs: tree.proc_count,
            system_percpu_pct: percpu_pct.clone(),
            tree_percpu_pct: percpu_pct,
            system_cpu_efficiency: cpu_pct_sum,
            cpu_efficiency_pct: cpu_efficiency_pct(cpu_pct_sum, self.allowed_cpus.len()),
            booked_walltime: self.args.booked_walltime_normalised(),
            booked_walltime_sec: self.args.booked_walltime_seconds(),
            booked_mem: self.booked_mem_display.clone(),
            booked_mem_bytes: self.booked_mem_bytes,
            n_threads: tree.thread_count,
            n_open_fds: tree.fd_count,
            major_faults: tree.major_faults,
            minor_faults: tree.minor_faults,
            io_read_bytes: tree.io_read_bytes,
            io_write_bytes: tree.io_write_bytes,
            io_read_ops: tree.io_read_ops,
            io_write_ops: tree.io_write_ops,
            swap_bytes: mem
                .as_ref()
                .and_then(|m| m.swap_bytes)
                .unwrap_or(tree.swap_bytes),
            cgroup_mem_bytes: mem.as_ref().map(|m| m.current_bytes),
            cgroup_mem_peak_bytes: mem.as_ref().and_then(|m| m.peak_bytes),
            cgroup_swap_bytes: mem.as_ref().and_then(|m| m.swap_bytes),
            cgroup_io_read_bytes: io.as_ref().map(|i| i.read_bytes),
            cgroup_io_write_bytes: io.as_ref().map(|i| i.write_bytes),
            cgroup_io_read_ops: io.as_ref().map(|i| i.read_ops),
            cgroup_io_write_ops: io.as_ref().map(|i| i.write_ops),
            num_nodes: self.summary.num_nodes,
            tasks_per_node: self.summary.tasks_per_node,
            cpus_per_task: self.summary.cpus_per_task,
            net_recv_bytes: net_delta.recv_bytes,
            net_sent_bytes: net_delta.sent_bytes,
            net_recv_packets: net_delta.recv_packets,
            net_sent_packets: net_delta.sent_packets,
            // /proc/net/dev has no notion of job ownership, and there is no
            // cheap per-job alternative without net_cls or eBPF, so these stay
            // node-wide and say so.
            net_is_node_scoped: true,
            gpu_utilization: gpu_stats.utilization,
            gpu_memory_used: gpu_stats.memory_used,
            gpu_memory_total: gpu_stats.memory_total,
            gpu_temperature: gpu_stats.temperature,
            gpu_power: gpu_stats.power,
            gpu_indices: gpu_stats.indices,
            // False once CUDA_VISIBLE_DEVICES has let us narrow the readings to
            // the GPUs actually assigned to this job.
            gpu_is_node_scoped: !gpu_stats.job_scoped,
        })
    }

    fn update_summary(&mut self, sample: &TelemetrySample) {
        let s = &mut self.summary;

        s.cpu_pct_sum_peak = s.cpu_pct_sum_peak.max(sample.cpu_pct_sum);
        s.cpu_efficiency_pct_peak = s.cpu_efficiency_pct_peak.max(sample.cpu_efficiency_pct);
        s.rss_bytes_peak = s.rss_bytes_peak.max(sample.rss_bytes_sum);
        // Derived from the peak, not the current sample. This line used to
        // assign the current value, so the "peak" reported was whatever the job
        // happened to be using in its final tick.
        s.rss_gb_peak = s.rss_bytes_peak as f64 / (1024.0 * 1024.0 * 1024.0);
        s.n_procs_peak = s.n_procs_peak.max(sample.n_procs);
        s.n_threads_peak = s.n_threads_peak.max(sample.n_threads);
        s.n_open_fds_peak = s.n_open_fds_peak.max(sample.n_open_fds);
        s.swap_bytes_peak = s.swap_bytes_peak.max(sample.swap_bytes);

        // cgroup memory: take the max of what we observed and the kernel's own
        // high-water mark, which also catches spikes between our samples.
        if let Some(current) = sample.cgroup_mem_bytes {
            s.cgroup_mem_peak_bytes = Some(s.cgroup_mem_peak_bytes.unwrap_or(0).max(current));
        }
        if let Some(kernel_peak) = sample.cgroup_mem_peak_bytes {
            s.cgroup_mem_peak_bytes = Some(s.cgroup_mem_peak_bytes.unwrap_or(0).max(kernel_peak));
        }
        if let Some(peak) = s.cgroup_mem_peak_bytes {
            s.cgroup_mem_peak_gb = Some(peak as f64 / (1024.0 * 1024.0 * 1024.0));
        }
        if let Some(swap) = sample.cgroup_swap_bytes {
            s.cgroup_swap_peak_bytes = Some(s.cgroup_swap_peak_bytes.unwrap_or(0).max(swap));
        }

        // Legacy per-process counters: these are summed over *live* processes,
        // so they fall when children exit. Assigning the latest value (as before)
        // meant a job that forked workers and reaped them reported near-zero
        // lifetime I/O. Taking the running max is the best that can be done
        // from this data source.
        s.major_faults_total = s.major_faults_total.max(sample.major_faults);
        s.minor_faults_total = s.minor_faults_total.max(sample.minor_faults);
        s.io_read_bytes_total = s.io_read_bytes_total.max(sample.io_read_bytes);
        s.io_write_bytes_total = s.io_write_bytes_total.max(sample.io_write_bytes);
        s.io_read_ops_total = s.io_read_ops_total.max(sample.io_read_ops);
        s.io_write_ops_total = s.io_write_ops_total.max(sample.io_write_ops);

        // cgroup I/O counters are genuinely cumulative and monotonic, so the
        // latest reading is the total.
        if let Some(v) = sample.cgroup_io_read_bytes {
            s.cgroup_io_read_bytes_total = Some(v);
        }
        if let Some(v) = sample.cgroup_io_write_bytes {
            s.cgroup_io_write_bytes_total = Some(v);
        }
        if let Some(v) = sample.cgroup_io_read_ops {
            s.cgroup_io_read_ops_total = Some(v);
        }
        if let Some(v) = sample.cgroup_io_write_ops {
            s.cgroup_io_write_ops_total = Some(v);
        }

        s.net_recv_bytes_total += sample.net_recv_bytes;
        s.net_sent_bytes_total += sample.net_sent_bytes;
        s.net_recv_packets_total += sample.net_recv_packets;
        s.net_sent_packets_total += sample.net_sent_packets;

        max_into_f64(&mut s.gpu_utilization_peak, &sample.gpu_utilization);
        max_into_u64(&mut s.gpu_memory_used_peak, &sample.gpu_memory_used);
        max_into_u64(&mut s.gpu_memory_total, &sample.gpu_memory_total);
        max_into_f64(&mut s.gpu_temperature_peak, &sample.gpu_temperature);
        max_into_f64(&mut s.gpu_power_peak, &sample.gpu_power);
        if !sample.gpu_indices.is_empty() {
            s.gpu_indices = sample.gpu_indices.clone();
        }
        s.gpu_is_node_scoped = sample.gpu_is_node_scoped;

        // Per-CPU vectors grow to the node's CPU count rather than being capped
        // at allowed_cpus.len(), which silently discarded every CPU past the
        // first N and misattributed the rest.
        max_into_f64(&mut s.system_percpu_pct_peak, &sample.system_percpu_pct);

        // Use the sample's own measured window, not the nominal interval. The
        // tick always runs slightly longer than requested, so charging CPU
        // seconds at the nominal rate under-reported every job.
        let dt = if sample.dt_sec > 0.0 {
            sample.dt_sec
        } else {
            self.args.interval
        };
        s.cpu_core_seconds += sample.cpu_pct_sum * dt / 100.0;
        add_scaled_into(
            &mut s.cpu_core_seconds_by_cpu,
            &sample.system_percpu_pct,
            dt / 100.0,
        );
    }

    fn emit_job_start(&mut self) -> Result<()> {
        let now = chrono::Local::now();
        let t = now.timestamp() as f64 + now.timestamp_subsec_millis() as f64 / 1000.0;

        let event = json!({
            "event": "job_start",
            "user_id": self.args.get_user_id(),
            "job_id": self.args.get_job_id(),
            "queue": self.args.queue,
            "job_name": self.args.job_name,
            "project": self.args.project,
            "t": t,
            // Same representation as job_log. These two events used to disagree:
            // job_start emitted "4294967296b" where job_log emitted "4GB".
            "booked_walltime": self.args.booked_walltime_normalised(),
            "booked_walltime_sec": self.args.booked_walltime_seconds(),
            "booked_mem": self.booked_mem_display,
            "booked_mem_bytes": self.booked_mem_bytes,
            "booked_mem_source": self.booked_mem_source,
            // Whether booked_mem_bytes describes this node or the whole job.
            // A consumer summing across nodes must only do so when true.
            "booked_mem_is_per_node": self.booked_mem_is_per_node,
            "allowed_cpus": self.summary.allowed_cpus,
            "num_nodes": self.summary.num_nodes,
            "tasks_per_node": self.summary.tasks_per_node,
            "cpus_per_task": self.summary.cpus_per_task,
            "cgroup_version": self.summary.cgroup_version,
            "logger_version": env!("CARGO_PKG_VERSION"),
            "hostname": self.hostname,
            "node_rank": self.args.node_rank,
        });

        writeln!(self.output_file, "{}", event)?;
        self.output_file.flush()?;
        Ok(())
    }

    fn emit_job_end(&mut self) -> Result<()> {
        let now = chrono::Local::now();
        let t = now.timestamp() as f64 + now.timestamp_subsec_millis() as f64 / 1000.0;

        let (exit_status, exit_reason) = self.read_exit_status();
        self.summary.exit_status = exit_status;
        self.summary.exit_reason = exit_reason.clone();

        let duration = self.start_time.elapsed().as_secs_f64();
        let core_hours = self.summary.cpu_core_seconds / 3600.0;

        let event = json!({
            "event": "job_end",
            "user_id": self.args.get_user_id(),
            "job_id": self.args.get_job_id(),
            "t": t,
            // null, not 0, when we genuinely do not know. Reporting 0 made every
            // job look successful and made failure analysis impossible.
            "exit_status": exit_status,
            "exit_reason": exit_reason,
            "duration_sec": duration,
            "samples": self.samples,
            "cpu_core_seconds": self.summary.cpu_core_seconds,
            // Service Units are core-hours times the queue's charge rate. The
            // rate table is the dashboard's business, not the logger's.
            "cpu_core_hours": core_hours,
        });

        writeln!(self.output_file, "{}", event)?;
        self.output_file.flush()?;
        Ok(())
    }

    /// True once the head node has recorded the workload's exit status.
    ///
    /// This is how loggers on remote nodes learn the job is over. Returns false
    /// when no exit-status file was configured, so a logger started by hand
    /// keeps running until it is signalled.
    fn workload_has_finished(&self) -> bool {
        self.args
            .exit_status_file
            .as_ref()
            .is_some_and(|path| path.exists())
    }

    /// Read the workload's exit status, if the wrapper recorded one.
    fn read_exit_status(&self) -> (Option<i32>, String) {
        let Some(path) = &self.args.exit_status_file else {
            return (
                None,
                "unknown: no --exit-status-file supplied (use logger-rs.sh)".to_string(),
            );
        };

        let Ok(content) = std::fs::read_to_string(path) else {
            return (
                None,
                "unknown: exit status file absent; the job was probably killed before it \
                 could be written"
                    .to_string(),
            );
        };

        match content.trim().parse::<i32>() {
            Ok(code) => (Some(code), describe_exit_code(code)),
            Err(_) => (
                None,
                format!("unknown: could not parse exit status file {path:?}"),
            ),
        }
    }

    /// Bring the derived fields of `self.summary` up to date with the run so far.
    ///
    /// Split out of `write_final_summary` so a mid-run checkpoint produces the
    /// same shape of document as a clean exit. Everything here is a pure
    /// function of the accumulators and the elapsed time, so calling it
    /// repeatedly is harmless: each call overwrites the previous derivation
    /// rather than adding to it.
    fn recompute_derived_fields(&mut self) {
        let duration = self.start_time.elapsed().as_secs_f64();

        self.summary.samples = self.samples;
        self.summary.duration_sec = duration;
        self.summary.t_end = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%z")
            .to_string();
        self.summary.cpu_core_hours = self.summary.cpu_core_seconds / 3600.0;

        if duration > 0.0 {
            let avg_pct = (self.summary.cpu_core_seconds / duration) * 100.0;
            let booked_cores = self.summary.allowed_cpus.len();
            let by_cpu: Vec<f64> = self
                .summary
                .cpu_core_seconds_by_cpu
                .iter()
                .map(|secs| (secs / duration) * 100.0)
                .collect();

            self.summary.avg_cpu_percent_over_run = avg_pct;
            self.summary.cpu_efficiency_pct_avg = cpu_efficiency_pct(avg_pct, booked_cores);
            self.summary.tree_avg_cpu_percent_by_cpu = by_cpu;
        }

        // cpuacct cannot attribute per-CPU time to individual processes, so the
        // process-tree view is the system view. Previously this was allocated
        // and never written, so it was always a vector of zeros.
        let system_peaks = self.summary.system_percpu_pct_peak.clone();
        self.summary.tree_percpu_pct_peak = system_peaks;

        let booked_walltime_sec = self.summary.booked_walltime_sec;
        if booked_walltime_sec > 0 {
            self.summary.walltime_efficiency_pct = (duration / booked_walltime_sec as f64) * 100.0;
        }

        let peak_mem = self.summary.effective_peak_mem_bytes();
        let booked_mem_bytes = self.summary.booked_mem_bytes;
        if booked_mem_bytes > 0 {
            self.summary.mem_efficiency_pct = (peak_mem as f64 / booked_mem_bytes as f64) * 100.0;
        }
    }

    /// Write a mid-run checkpoint of the summary.
    ///
    /// This exists because of how jobs actually end. A job that exhausts its
    /// walltime gets SIGTERM and then, a few seconds later, SIGKILL — and
    /// SIGKILL cannot be caught, so neither the shutdown path nor `Drop` runs.
    /// With the summary written only at exit, every node in that job left no
    /// summary at all and `--merge` had nothing to combine, even though the
    /// NDJSON stream beside it held the whole run. A checkpoint bounds that
    /// loss to one interval instead of the entire job.
    ///
    /// Deliberately *not* setting `summary_written`: this is a placeholder for
    /// a final write that we still expect to happen.
    ///
    /// Exit status is left alone. `read_exit_status` would find no file yet and
    /// record a parse failure into `exit_reason`, which would then no longer
    /// equal "unknown" and would stop the final write from filling it in.
    fn write_checkpoint_summary(&mut self) -> Result<()> {
        if self.summary_written {
            return Ok(());
        }
        self.recompute_derived_fields();
        self.summary.partial = true;
        write_summary_file(&self.summary_path, &self.summary)
    }

    fn write_final_summary(&mut self) -> Result<()> {
        if self.summary_written {
            return Ok(());
        }

        self.recompute_derived_fields();

        if self.summary.exit_reason == "unknown" {
            let (status, reason) = self.read_exit_status();
            self.summary.exit_status = status;
            self.summary.exit_reason = reason;
        }

        // Clears the flag a checkpoint may have set, so the file on disk always
        // says whether it describes a complete run.
        self.summary.partial = false;

        write_summary_file(&self.summary_path, &self.summary)?;
        self.summary_written = true;

        eprintln!("Summary written to: {:?}", self.summary_path);
        Ok(())
    }
}

/// Work out how much memory this job booked, and whether the figure is
/// per-node.
///
/// The cgroup limit is preferred over the PBS environment for two reasons.
/// First, Gadi exports neither `PBS_RESOURCE_LIST_mem` nor `PBS_RESOURCE_mem`,
/// so the environment usually yields nothing at all and memory could only be
/// shown in absolute GB. Second, the cgroup limit is unambiguously **per
/// node**, which is what a multi-node consumer needs in order to sum it — a
/// `-l mem=` value is the whole job's, and summing it across nodes would
/// overstate the booking by the node count.
///
/// Returns `(bytes, display string, source, is_per_node)`.
fn resolve_booked_mem(args: &Args, cgroup: Option<&Cgroup>) -> (u64, String, String, bool) {
    let cgroup_limit = cgroup
        .and_then(|c| c.read_memory())
        .and_then(|m| m.limit_bytes);

    if let Some(bytes) = cgroup_limit {
        return (
            bytes,
            format_bytes_human(bytes),
            "cgroup-limit".to_string(),
            true,
        );
    }

    let from_env = args.booked_mem_bytes();
    if from_env > 0 {
        return (
            from_env,
            args.booked_mem_normalised(),
            "pbs-env".to_string(),
            // A `-l mem=` booking covers the whole job, not one node.
            false,
        );
    }

    (0, "0b".to_string(), "unknown".to_string(), false)
}

/// Render a byte count the way an operator would write it, so it round-trips
/// through the same parser that reads `-l mem=` values.
pub fn format_bytes_human(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    if bytes >= GIB && bytes.is_multiple_of(GIB) {
        format!("{}GB", bytes / GIB)
    } else if bytes >= MIB && bytes.is_multiple_of(MIB) {
        format!("{}MB", bytes / MIB)
    } else {
        format!("{}b", bytes)
    }
}

/// Turn a shell exit code into something a human can read.
pub fn describe_exit_code(code: i32) -> String {
    match code {
        0 => "completed".to_string(),
        // The shell reports a signal death as 128 + signal number.
        130 => "terminated: SIGINT".to_string(),
        137 => "killed: SIGKILL (out of memory, or walltime hard limit)".to_string(),
        143 => "terminated: SIGTERM (walltime exceeded, or qdel)".to_string(),
        c if c > 128 => format!("terminated by signal {}", c - 128),
        c => format!("failed with exit status {c}"),
    }
}

impl Drop for TelemetryLogger {
    fn drop(&mut self) {
        // Last-resort summary write if run() did not get to finish, e.g. on a
        // panic. Guarded by summary_written so the normal path does not
        // serialise and rename the file twice.
        //
        // This only works because the release profile does NOT set
        // panic = "abort" — aborting skips unwinding, and Drop never runs.
        if !self.summary_written {
            if let Err(e) = self.write_final_summary() {
                eprintln!("WARNING: could not write summary during shutdown: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_bytes_human_round_trips() {
        // Must parse back through the same reader that handles `-l mem=`
        // values, or the string and the numeric field would disagree.
        assert_eq!(format_bytes_human(64 * 1024 * 1024 * 1024), "64GB");
        assert_eq!(format_bytes_human(192 * 1024 * 1024 * 1024), "192GB");
        assert_eq!(format_bytes_human(512 * 1024 * 1024), "512MB");
        // Not a clean multiple — fall back to an exact byte count rather than
        // rounding and reporting a booking the job never had.
        assert_eq!(format_bytes_human(1234567), "1234567b");
        assert_eq!(format_bytes_human(0), "0b");
    }

    #[test]
    fn test_describe_exit_code() {
        assert_eq!(describe_exit_code(0), "completed");
        assert!(describe_exit_code(1).contains("exit status 1"));
        assert!(describe_exit_code(143).contains("SIGTERM"));
        assert!(describe_exit_code(137).contains("SIGKILL"));
        assert!(describe_exit_code(160).contains("signal 32"));
    }
}
