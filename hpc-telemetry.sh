#!/bin/bash
#
# Telemetry collection for an HPC job, driven from inside your own PBS script.
#
# Usage:
#
#   module use /g/data/gb02/modules
#   module load hpc-telemetry
#   source hpc-telemetry.sh
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
#   --telemetry-bin PATH hpc-telemetry binary (default: found on $PATH)
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
    echo "hpc-telemetry.sh is meant to be sourced, not executed:" >&2
    echo "" >&2
    echo "    source hpc-telemetry.sh" >&2
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
_HT_OUTPUT=""
_HT_OUTPUT_DIR=""
_HT_SINGLE_NODE=0
_HT_DO_MERGE=1
_HT_JOB_ID=""
_HT_BOOKED_WALLTIME=""
_HT_EXIT_STATUS_FILE=""
_HT_LOCAL_PID=""
_HT_LAUNCHER_PIDS=()
_HT_NODES=()
_HT_NODES_FULL=()
_HT_NUM_NODES=0

_ht_warn() {
    echo "hpc-telemetry: $*" >&2
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
            --procs-from cgroup \
            --job-id "$_HT_JOB_ID" \
            ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
            --node-rank "$rank" \
            --exit-status-file "$_HT_EXIT_STATUS_FILE" &
        _HT_LAUNCHER_PIDS+=($!)
    elif command -v pbsdsh >/dev/null 2>&1; then
        pbsdsh -n "$rank" -- \
            "$_HT_BIN" \
            --outfile "$_HT_OUTPUT" \
            --interval "$_HT_INTERVAL" \
            --gpu-interval "$_HT_GPU_INTERVAL" \
            --procs-from cgroup \
            --job-id "$_HT_JOB_ID" \
            ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
            --node-rank "$rank" \
            --exit-status-file "$_HT_EXIT_STATUS_FILE" &
        _HT_LAUNCHER_PIDS+=($!)
    elif command -v srun >/dev/null 2>&1; then
        srun --nodes=1 --ntasks=1 --nodelist="$host" --overlap \
            "$_HT_BIN" \
            --outfile "$_HT_OUTPUT" \
            --interval "$_HT_INTERVAL" \
            --gpu-interval "$_HT_GPU_INTERVAL" \
            --procs-from cgroup \
            --job-id "$_HT_JOB_ID" \
            ${_HT_BOOKED_WALLTIME:+--booked-walltime "$_HT_BOOKED_WALLTIME"} \
            --node-rank "$rank" \
            --exit-status-file "$_HT_EXIT_STATUS_FILE" &
        _HT_LAUNCHER_PIDS+=($!)
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

    _HT_BIN="${TELEMETRY_BIN:-hpc-telemetry}"
    _HT_INTERVAL="0.5"
    _HT_GPU_INTERVAL="5.0"
    _HT_OUTPUT=""
    _HT_SINGLE_NODE=0
    _HT_DO_MERGE=1
    _HT_LOCAL_PID=""
    _HT_LAUNCHER_PIDS=()
    _HT_NODES=()
    _HT_NODES_FULL=()
    _HT_NUM_NODES=0

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --output)        _HT_OUTPUT="${2:-}"; shift 2 ;;
            --interval)      _HT_INTERVAL="${2:-}"; shift 2 ;;
            --gpu-interval)  _HT_GPU_INTERVAL="${2:-}"; shift 2 ;;
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
    if ! command -v "$_HT_BIN" >/dev/null 2>&1 && [[ ! -x "$_HT_BIN" ]]; then
        _ht_warn "binary '$_HT_BIN' not found; continuing WITHOUT telemetry."
        _ht_warn "  on Gadi: module use /g/data/gb02/modules && module load hpc-telemetry"
        return 0
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
        _HT_EXIT_STATUS_FILE="${_HT_OUTPUT_DIR}/.hpc-telemetry-exit_${_HT_JOB_ID}"
    else
        _HT_EXIT_STATUS_FILE="$(mktemp "${TMPDIR:-/tmp}/hpc-telemetry-exit.XXXXXX")"
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
    # summaries. They poll every couple of seconds, so this is generous.
    if [[ "${#_HT_LAUNCHER_PIDS[@]}" -gt 0 ]]; then
        _ht_warn "waiting for $((_HT_NUM_NODES - 1)) remote logger(s)..."
        local _try _pid _still
        for _try in $(seq 1 30); do
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
            _ht_warn "merge failed; the per-node summaries are still on disk"
        fi
    fi

    rm -f "$_HT_EXIT_STATUS_FILE"

    # Preserves `set -e` semantics for the caller: `telemetry_stop` after a
    # failed command returns that command's status, so the script still fails.
    return "$status"
}
