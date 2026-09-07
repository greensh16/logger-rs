#!/bin/bash
#
# Telemetry collection for an HPC job, driven from inside your own PBS script.
#
# Usage:
#
#   module use /g/data/gb02/modules
#   module load logger-rs
#   source logger-rs.sh
#
#   cp -r /g/data/ab12/inputs "$PBS_JOBFS"/      # not measured
#
#   telemetry_start
#       mpirun -np "$PBS_NCPUS" ./my_model       # measured
#       ./postprocess                            # measured
#   telemetry_stop
#
#   cp -r "$PBS_JOBFS"/out /g/data/ab12/         # not measured
#
# Options to telemetry_start:
#   --output PATH        NDJSON output path. May contain {host}, {jobid} and
#                        {user}, expanded on each node. For multi-node jobs
#                        _{host} is inserted automatically if absent, so the
#                        nodes cannot overwrite each other.
#                        (default: $HPC_DASHBOARD_DIR/psutil_{jobid}_{host}.log,
#                        or the working directory if that is unset)
#   --interval SECS      Sampling interval (default: 0.5)
#   --gpu-interval SECS  GPU polling interval (default: 5.0)
#   --summary-every SECS How often each node checkpoints its summary, so a
#                        walltime kill still leaves something to merge
#                        (default: 30, 0 disables)
#   --telemetry-bin PATH logger-rs binary (default: found on $PATH)
#   --single-node        Only measure this node, even on a multi-node job
#   --no-merge           Skip the job-level merge at the end
#
# Why this shape rather than wrapping your command:
#
#   Prefixing a command with a launcher stops working the moment a job script
#   has a pipeline, a redirection, several separate runs, or a command line
#   assembled in variables — which is most of them. Delimiting a region leaves
#   your script exactly as it was.
#
#   It also lets you exclude data staging. A twenty-minute copy from /g/data to
#   jobfs, measured, looks like your job running at nearly zero CPU efficiency,
#   which drags down every figure and makes the dashboard recommend fewer cores
#   than the science actually needs.
#
# telemetry_stop is optional: if the script ends, exits early under `set -e`, or
# is terminated, an EXIT trap performs the same shutdown. Calling it explicitly
# is better where you can, because it captures the exit status of your workload
# directly and lets you leave clean-up out of the measurement.
#
# Nothing here can fail your job. Every setup problem is reported and then
# ignored — a job that runs unmeasured is far better than a job that does not
# run.

# ---------------------------------------------------------------------------
# This file is a library. Executing it does nothing useful.
# ---------------------------------------------------------------------------
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    echo "logger-rs.sh is meant to be sourced, not executed:" >&2
    echo "" >&2
    echo "    source logger-rs.sh" >&2
    echo "    telemetry_start" >&2
    echo "    <your commands>" >&2
    echo "    telemetry_stop" >&2
    exit 64
fi

# ---------------------------------------------------------------------------
# Internal state.
#
# Everything is prefixed `_HT_` / `_ht_`, because sourcing drops these straight
# into the user's shell. The previous wrapper could afford plain names like
# OUTPUT, NODES and cleanup() when it ran as its own process; here a name like
# `cleanup` would silently replace a function of the same name in the job
# script — which plenty of scripts define — and break it in a way nobody would
# think to attribute to the telemetry.
# ---------------------------------------------------------------------------
_HT_ACTIVE=0
_HT_BIN=""
_HT_INTERVAL="0.5"
_HT_GPU_INTERVAL="5.0"
_HT_SUMMARY_EVERY="30"
_HT_OUTPUT=""
_HT_OUTPUT_DIR=""
_HT_SINGLE_NODE=0
_HT_DO_MERGE=1
_HT_JOB_ID=""
_HT_BOOKED_WALLTIME=""
_HT_EXIT_STATUS_FILE=""
_HT_LOCAL_PID=""
_HT_LAUNCHER_PIDS=()
_HT_LAUNCHER_HOSTS=()
_HT_NODES=()
_HT_NODES_FULL=()
_HT_NUM_NODES=0

_ht_warn() {
    echo "logger-rs: $*" >&2
}

# The booked walltime, which PBS does not put in the environment.
#
# Gadi exports none of PBS_RESOURCE_LIST_walltime, PBS_WALLTIME or
# PBS_RESOURCE_walltime — the same gap that forced booked *memory* to be read
# from the cgroup. Memory at least has a kernel-side source; walltime has none,
# so the only place the booking exists is the scheduler itself.
#
# Without it `booked_walltime_sec` is 0, and the dashboard cannot tell a job
# that used 5% of its booking from one whose booking is simply unknown.
#
# Asked once here, on the head node, and passed to every node's logger — rather
# than each logger shelling out to qstat itself, which would be one fork per
# node for an answer that is identical across the job.
#
# Every failure path is silent: qstat missing, a format this does not parse, a
# hung scheduler. The result is the status quo (unknown), never a broken job.
_ht_booked_walltime() {
    [[ -n "${PBS_JOBID:-}" ]] || return 0
    command -v qstat >/dev/null 2>&1 || return 0

    local out
    # A scheduler under load can take a while to answer, and this sits between
    # the user's script and their workload starting. Two seconds or nothing.
    out=$(timeout 2 qstat -f "$PBS_JOBID" 2>/dev/null) || return 0

    # `qstat -f` wraps long lines by continuing them with a leading tab, so the
    # value is unfolded before matching.
    printf '%s\n' "$out" \
        | tr -d '\n' | sed 's/\t//g' \
        | grep -oE 'Resource_List\.walltime = [0-9]+:[0-9]{2}:[0-9]{2}' \
        | head -1 \
        | awk '{print $3}'
}

# PBS_NODEFILE lists one line per requested CPU, so the same host appears many
# times; we want one logger per host, not per core.
_ht_node_list_full() {
    if [[ -n "${PBS_NODEFILE:-}" && -r "${PBS_NODEFILE}" ]]; then
        awk '{print $1}' "$PBS_NODEFILE" | sort -u
    elif [[ -n "${SLURM_JOB_NODELIST:-}" ]] && command -v scontrol >/dev/null 2>&1; then
        scontrol show hostnames "$SLURM_JOB_NODELIST" | sort -u
    fi
}

_ht_start_logger_here() {
    # --tree-pid "$$" roots the process tree at the job script, so everything it
    # spawns is counted. Under the old wrapper this was the wrapper's own PID;
    # sourced, "$$" is the user's script, which is if anything more accurate.
    "$_HT_BIN" \
        --outfile "$_HT_OUTPUT" \
        --interval "$_HT_INTERVAL" \
        --gpu-interval "$_HT_GPU_INTERVAL" \
        --summary-every "$_HT_SUMMARY_EVERY" \
        --tree-pid "$$" \
        --job-id "$_HT_JOB_ID" \
        ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
        --node-rank 0 \
        --exit-status-file "$_HT_EXIT_STATUS_FILE" &
    _HT_LOCAL_PID=$!
}

_ht_start_logger_remote() {
    local rank="$1" host="$2" host_full="$3"

    # No --tree-pid here, deliberately.
    #
    # On a remote node the job's ranks are started by the scheduler, not by us,
    # so there is no process tree of ours to walk. Passing --tree-pid 1 (as an
    # earlier version did) was actively wrong: /proc/1/cgroup is the *root*
    # cgroup, so CPU and memory would have described the whole node rather than
    # the job, and a tree walk from init would have counted every process on the
    # machine.
    #
    # pbs_tmrsh is preferred over pbsdsh: it targets a host by name, whereas
    # `pbsdsh -n N` indexes PBS's *vnode* list, which does not necessarily line
    # up one-to-one with unique hostnames. Getting that mapping wrong would put
    # two loggers on one node and none on another.
    if command -v pbs_tmrsh >/dev/null 2>&1; then
        pbs_tmrsh "$host_full" \
            "$_HT_BIN" \
            --outfile "$_HT_OUTPUT" \
            --interval "$_HT_INTERVAL" \
            --gpu-interval "$_HT_GPU_INTERVAL" \
            --summary-every "$_HT_SUMMARY_EVERY" \
            --procs-from cgroup \
            --job-id "$_HT_JOB_ID" \
            ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
            --node-rank "$rank" \
            --exit-status-file "$_HT_EXIT_STATUS_FILE" &
        _HT_LAUNCHER_PIDS+=($!)
        _HT_LAUNCHER_HOSTS+=("$host")
    elif command -v pbsdsh >/dev/null 2>&1; then
        pbsdsh -n "$rank" -- \
            "$_HT_BIN" \
            --outfile "$_HT_OUTPUT" \
            --interval "$_HT_INTERVAL" \
            --gpu-interval "$_HT_GPU_INTERVAL" \
            --summary-every "$_HT_SUMMARY_EVERY" \
            --procs-from cgroup \
            --job-id "$_HT_JOB_ID" \
            ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
            --node-rank "$rank" \
            --exit-status-file "$_HT_EXIT_STATUS_FILE" &
        _HT_LAUNCHER_PIDS+=($!)
        _HT_LAUNCHER_HOSTS+=("$host")
    elif command -v srun >/dev/null 2>&1; then
        srun --nodes=1 --ntasks=1 --nodelist="$host" --overlap \
            "$_HT_BIN" \
            --outfile "$_HT_OUTPUT" \
            --interval "$_HT_INTERVAL" \
            --gpu-interval "$_HT_GPU_INTERVAL" \
            --summary-every "$_HT_SUMMARY_EVERY" \
            --procs-from cgroup \
            --job-id "$_HT_JOB_ID" \
            ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
            --node-rank "$rank" \
            --exit-status-file "$_HT_EXIT_STATUS_FILE" &
        _HT_LAUNCHER_PIDS+=($!)
        _HT_LAUNCHER_HOSTS+=("$host")
    else
        _ht_warn "no pbs_tmrsh, pbsdsh or srun; cannot measure $host"
    fi
}

# ---------------------------------------------------------------------------
# telemetry_start — begin measuring here.
# ---------------------------------------------------------------------------
telemetry_start() {
    if [[ "$_HT_ACTIVE" -eq 1 ]]; then
        _ht_warn "telemetry_start called twice; ignoring the second call"
        return 0
    fi

    _HT_BIN="${TELEMETRY_BIN:-logger-rs}"
    _HT_INTERVAL="0.5"
    _HT_GPU_INTERVAL="5.0"
    _HT_SUMMARY_EVERY="30"
    _HT_OUTPUT=""
    _HT_SINGLE_NODE=0
    _HT_DO_MERGE=1
    _HT_LOCAL_PID=""
    _HT_LAUNCHER_PIDS=()
    _HT_LAUNCHER_HOSTS=()
    _HT_NODES=()
    _HT_NODES_FULL=()
    _HT_NUM_NODES=0

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --output)        _HT_OUTPUT="${2:-}"; shift 2 ;;
            --interval)      _HT_INTERVAL="${2:-}"; shift 2 ;;
            --gpu-interval)  _HT_GPU_INTERVAL="${2:-}"; shift 2 ;;
            --summary-every) _HT_SUMMARY_EVERY="${2:-}"; shift 2 ;;
            --telemetry-bin) _HT_BIN="${2:-}"; shift 2 ;;
            --single-node)   _HT_SINGLE_NODE=1; shift ;;
            --no-merge)      _HT_DO_MERGE=0; shift ;;
            *)
                _ht_warn "unknown option '$1' — ignoring it and carrying on"
                shift
                ;;
        esac
    done

    _HT_JOB_ID="${PBS_JOBID:-${SLURM_JOB_ID:-local_$$}}"

    # Asked once, here, and handed to every node's logger below. An empty
    # result means "not determined" and the flag is simply omitted, leaving the
    # logger's own environment fallbacks to try.
    _HT_BOOKED_WALLTIME="$(_ht_booked_walltime)"
    if [[ -n "$_HT_BOOKED_WALLTIME" ]]; then
        _ht_warn "booked walltime $_HT_BOOKED_WALLTIME (from qstat)"
    fi

    if [[ -z "$_HT_OUTPUT" ]]; then
        _HT_OUTPUT="${HPC_DASHBOARD_DIR:-$PWD}/psutil_{jobid}_{host}.log"
    fi

    # From here on, every failure returns 0. Telemetry must never be the reason
    # a job fails — the caller is a user's production script, and `set -e` is
    # common, so a non-zero return here would abort it.
    # Resolve to an ABSOLUTE path before anything else uses it.
    #
    # This must not stay a bare command name. The local logger is started by
    # bash, which searches PATH, so a bare name works here — but every remote
    # node is reached through pbs_tmrsh/pbsdsh/srun, and those hand the command
    # straight to execv(). execv does not search PATH. A bare name therefore
    # fails on every remote node with:
    #
    #     ERROR: Could not execv logger-rs! ret=-1 errno=2
    #
    # (errno 2 is ENOENT), leaving a multi-node job with telemetry from the
    # mother superior only — one log file where there should be N, and a
    # manifest promising nodes that never wrote anything.
    #
    # Even where PATH would nominally be inherited it cannot be relied on: the
    # remote environment is not the job script's, so a PATH entry added by
    # `module load` in the submitting shell is not necessarily present there.
    # Resolving here means the module only has to be loadable on this node.
    _ht_resolved=""
    if [[ "$_HT_BIN" == */* ]]; then
        # Already a path — make it absolute, since the remote node's working
        # directory is not guaranteed to be this one.
        if [[ -x "$_HT_BIN" ]]; then
            _ht_resolved="$(cd "$(dirname "$_HT_BIN")" 2>/dev/null && pwd)/$(basename "$_HT_BIN")"
        fi
    else
        _ht_resolved="$(command -v "$_HT_BIN" 2>/dev/null || true)"
    fi

    if [[ -z "$_ht_resolved" || ! -x "$_ht_resolved" ]]; then
        _ht_warn "binary '$_HT_BIN' not found; continuing WITHOUT telemetry."
        _ht_warn "  on Gadi: module use /g/data/gb02/modules && module load logger-rs"
        unset _ht_resolved
        return 0
    fi
    _HT_BIN="$_ht_resolved"
    unset _ht_resolved

    # A binary the other nodes cannot see fails exactly like a missing one, but
    # only on the remote nodes, and only at launch — which reads as "telemetry
    # is broken" rather than "this path is node-local". Worth naming up front.
    if [[ "$_HT_SINGLE_NODE" -eq 0 ]]; then
        case "$_HT_BIN" in
            /tmp/*|/var/tmp/*|/dev/shm/*|/local/*|"${PBS_JOBFS:-/nonexistent-jobfs}"/*)
                _ht_warn "'$_HT_BIN' looks node-local; other nodes will not see it."
                _ht_warn "  put it on shared storage (/g/data or /scratch) for multi-node jobs."
                ;;
        esac
    fi

    _HT_OUTPUT_DIR="$(dirname "$_HT_OUTPUT")"
    if ! mkdir -p "$_HT_OUTPUT_DIR" 2>/dev/null; then
        _ht_warn "cannot create '$_HT_OUTPUT_DIR'; continuing WITHOUT telemetry."
        _ht_warn "  if that is on /g/data or /scratch, the job needs"
        _ht_warn "  -l storage=gdata/<proj> (or scratch/<proj>)"
        return 0
    fi

    # Work out the allocation.
    if [[ "$_HT_SINGLE_NODE" -eq 0 ]]; then
        while IFS= read -r _ht_line; do
            [[ -n "$_ht_line" ]] && _HT_NODES_FULL+=("$_ht_line")
        done < <(_ht_node_list_full)
        unset _ht_line
    fi
    local _n
    for _n in "${_HT_NODES_FULL[@]:-}"; do
        [[ -n "$_n" ]] && _HT_NODES+=("${_n%%.*}")
    done
    _HT_NUM_NODES="${#_HT_NODES[@]}"

    # A multi-node output path must vary per node, or every logger writes to the
    # same file and the result is interleaved nonsense.
    if [[ "$_HT_NUM_NODES" -gt 1 && "$_HT_OUTPUT" != *"{host}"* ]]; then
        local _base="${_HT_OUTPUT%.*}" _ext="${_HT_OUTPUT##*.}"
        if [[ "$_base" == "$_HT_OUTPUT" ]]; then
            _HT_OUTPUT="${_HT_OUTPUT}_{host}"
        else
            _HT_OUTPUT="${_base}_{host}.${_ext}"
        fi
        _ht_warn "multi-node job; output template is now '$_HT_OUTPUT'"
    fi

    # The exit-status file must live on the shared filesystem for a multi-node
    # job: it is both the record of the outcome and the signal that tells
    # loggers on remote nodes to shut down, because pbsdsh and srun do not
    # reliably forward SIGTERM to the tasks they launched.
    if [[ "$_HT_NUM_NODES" -gt 1 ]]; then
        _HT_EXIT_STATUS_FILE="${_HT_OUTPUT_DIR}/.logger-rs-exit_${_HT_JOB_ID}"
    else
        _HT_EXIT_STATUS_FILE="$(mktemp "${TMPDIR:-/tmp}/logger-rs-exit.XXXXXX")"
    fi
    rm -f "$_HT_EXIT_STATUS_FILE"

    # Written before any logger starts, so a dashboard polling this directory
    # learns how many nodes to expect before the first sample appears. Written
    # for single-node jobs too, so the ingest path needs no special case.
    local _manifest_nodes=""
    if [[ "$_HT_NUM_NODES" -gt 1 ]]; then
        _manifest_nodes="$(printf '%s,' "${_HT_NODES[@]}")"
        _manifest_nodes="${_manifest_nodes%,}"
    fi
    if ! "$_HT_BIN" --write-manifest "$_HT_OUTPUT_DIR" \
            --output "$_HT_OUTPUT" --job-id "$_HT_JOB_ID" \
            ${_manifest_nodes:+--merge-expect-nodes "$_manifest_nodes"} 2>/dev/null; then
        _ht_warn "could not write the manifest; continuing anyway"
    fi

    _ht_start_logger_here

    if [[ "$_HT_NUM_NODES" -gt 1 ]]; then
        _ht_warn "measuring $_HT_NUM_NODES nodes"
        # Skip whichever entry is *this* node rather than assuming it sorts
        # first. PBS_NODEFILE is sorted alphabetically, so the mother superior
        # can appear anywhere in it; assuming index 0 would put two loggers on
        # one node and none on another.
        local _local_short _rank=0 _i
        _local_short="$(hostname -s 2>/dev/null || hostname | cut -d. -f1)"
        for _i in "${!_HT_NODES[@]}"; do
            if [[ "${_HT_NODES[$_i]}" == "$_local_short" ]]; then
                continue
            fi
            _rank=$((_rank + 1))
            _ht_start_logger_remote "$_rank" "${_HT_NODES[$_i]}" "${_HT_NODES_FULL[$_i]}"
        done
        if [[ "$_rank" -ne $((_HT_NUM_NODES - 1)) ]]; then
            _ht_warn "launched $_rank remote logger(s) for $_HT_NUM_NODES node(s);" \
                     "this node ('$_local_short') may not appear in PBS_NODEFILE under that name"
        fi
    fi

    # Give the local logger a moment, and say so if it died immediately.
    sleep 0.2
    if ! kill -0 "$_HT_LOCAL_PID" 2>/dev/null; then
        _ht_warn "the logger failed to start; continuing WITHOUT telemetry"
        _HT_LOCAL_PID=""
        return 0
    fi

    # Same check for the remote nodes, in the background.
    #
    # A remote launch that fails does so immediately — pbs_tmrsh/pbsdsh/srun
    # exit as soon as the exec fails — whereas a successful one stays alive for
    # the duration of the job, holding the remote logger. So "still running a
    # few seconds later" separates the two cleanly.
    #
    # Until this existed the only report of a failed remote launch was the
    # scheduler's own `Could not execv` line, buried in the job's stderr among
    # the workload's output, plus a merge warning hours later when the job
    # ended. On an 8-node job that meant discovering at the end that 7 nodes had
    # measured nothing.
    #
    # Backgrounded so the user's workload starts immediately: this is a
    # diagnostic, and it must not put a delay between telemetry_start and the
    # science. Output arrives a few seconds into the job, labelled.
    #
    # `disown` keeps bash from reporting the subshell in job-control output. The
    # subshell deliberately inherits stderr — that is where the warning has to
    # land for the user to ever see it.
    if [[ "${#_HT_LAUNCHER_PIDS[@]}" -gt 0 ]]; then
        (
            sleep 5
            _dead=()
            for _k in "${!_HT_LAUNCHER_PIDS[@]}"; do
                kill -0 "${_HT_LAUNCHER_PIDS[$_k]}" 2>/dev/null && continue
                _dead+=("${_HT_LAUNCHER_HOSTS[$_k]:-?}")
            done
            if [[ "${#_dead[@]}" -gt 0 ]]; then
                _ht_warn "${#_dead[@]} of ${#_HT_LAUNCHER_PIDS[@]} remote logger(s) failed to start: ${_dead[*]}"
                _ht_warn "  this node is still being measured; those nodes are not."
                _ht_warn "  the usual cause is the binary not being visible on the other nodes —"
                _ht_warn "  check the job's stderr for 'Could not execv', and that '$_HT_BIN'"
                _ht_warn "  is on storage the whole job can read (-l storage=gdata/<proj>)."
            fi
        ) &
        disown 2>/dev/null || true
    fi

    _HT_ACTIVE=1

    # Backstop for the cases telemetry_stop cannot cover: `set -e` aborting the
    # script, an early `exit`, or PBS sending SIGTERM at the walltime limit.
    # telemetry_stop disarms these when it runs normally.
    trap '_ht_on_exit' EXIT
    trap '_ht_on_signal 143' TERM
    trap '_ht_on_signal 130' INT

    _ht_warn "measuring; call telemetry_stop when done (or let the script end)"
    return 0
}

# Traps. `$?` is read as the very first statement — anything before it, even an
# assignment, overwrites the value that becomes the job's recorded outcome.
_ht_on_exit() {
    local status=$?
    telemetry_stop "$status"
    return "$status"
}

_ht_on_signal() {
    local forced="$1"
    telemetry_stop "$forced"
    exit "$forced"
}

# ---------------------------------------------------------------------------
# telemetry_stop — stop measuring, shut the loggers down, merge.
# ---------------------------------------------------------------------------
telemetry_stop() {
    # Captured first so that a bare `telemetry_stop` records the status of the
    # command immediately before it. An explicit argument (used by the traps)
    # wins.
    local status=$?
    if [[ $# -gt 0 ]]; then
        status="$1"
    fi

    if [[ "$_HT_ACTIVE" -ne 1 ]]; then
        return "$status"
    fi
    _HT_ACTIVE=0
    trap - EXIT TERM INT

    # Recorded before anything is shut down: this file is what the remote
    # loggers watch for, and what every logger reads to fill in exit_status.
    echo "$status" > "$_HT_EXIT_STATUS_FILE" 2>/dev/null

    if [[ -n "$_HT_LOCAL_PID" ]]; then
        kill -TERM "$_HT_LOCAL_PID" 2>/dev/null
        wait "$_HT_LOCAL_PID" 2>/dev/null
        _HT_LOCAL_PID=""
    fi

    # Wait for the remote loggers to notice the exit-status file and write their
    # summaries. They poll every couple of seconds, so 30s is generous.
    #
    # Unless we are ourselves being killed. A status of 143 or 137 means the
    # scheduler sent us SIGTERM and is now counting down to SIGKILL — on PBS Pro
    # that grace period is `kill_delay`, ten seconds by default. Waiting the full
    # 30s in that situation does not get us more summaries: it guarantees we are
    # killed before reaching the merge below, which is exactly how a walltime
    # kill ends up producing no job-level summary at all. Better to take
    # whatever the remotes have managed and spend the remaining seconds merging.
    #
    # The remotes are in the same race on their own nodes, which is why they
    # also checkpoint their summaries as they go (see --summary-every); this
    # wait is about catching a clean finish, not about rescuing a doomed one.
    local _wait_secs=30
    case "$status" in
        137|143) _wait_secs=4 ;;
    esac
    if [[ "${#_HT_LAUNCHER_PIDS[@]}" -gt 0 ]]; then
        _ht_warn "waiting up to ${_wait_secs}s for $((_HT_NUM_NODES - 1)) remote logger(s)..."
        local _try _pid _still
        for _try in $(seq 1 "$_wait_secs"); do
            _still=0
            for _pid in "${_HT_LAUNCHER_PIDS[@]}"; do
                kill -0 "$_pid" 2>/dev/null && _still=1
            done
            [[ "$_still" -eq 0 ]] && break
            sleep 1
        done
        for _pid in "${_HT_LAUNCHER_PIDS[@]}"; do
            kill -TERM "$_pid" 2>/dev/null
        done
        wait "${_HT_LAUNCHER_PIDS[@]}" 2>/dev/null
        _HT_LAUNCHER_PIDS=()
    fi

    if [[ "$_HT_DO_MERGE" -eq 1 ]]; then
        local _expect=""
        if [[ "$_HT_NUM_NODES" -gt 1 ]]; then
            _expect="$(printf '%s,' "${_HT_NODES[@]}")"
            _expect="${_expect%,}"
        fi
        # A merge failure must not change the job's exit status — telemetry is
        # secondary to the workload's own result.
        if ! "$_HT_BIN" --merge "$_HT_OUTPUT_DIR" --job-id "$_HT_JOB_ID" \
                ${_expect:+--merge-expect-nodes "$_expect"}; then
            _ht_warn "merge failed (see above); the per-node logs are still on disk."
            _ht_warn "you can retry it later from a login node:"
            _ht_warn "  logger-rs --merge $_HT_OUTPUT_DIR --job-id $_HT_JOB_ID"
        fi
    fi

    rm -f "$_HT_EXIT_STATUS_FILE"

    # Preserves `set -e` semantics for the caller: `telemetry_stop` after a
    # failed command returns that command's status, so the script still fails.
    return "$status"
}
