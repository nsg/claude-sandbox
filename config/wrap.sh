#!/bin/bash
# Wrapped tmux session helpers, installed as wrap, wrap-type, wrap-key and
# wrap-read (symlinks dispatch on the invocation name). The host-side
# `claude-sandbox wrap-*` commands forward here via podman exec, so this
# script is the single implementation of the wrap behaviour.
#
# The session name must match WRAP_TMUX_SESSION in claude-sandbox/src/main.rs.
set -euo pipefail

SESSION="${CLAUDE_WRAP_SESSION:-claude-sandbox}"

die() {
    echo "Error: $*" >&2
    exit 1
}

require_session() {
    tmux has-session -t "$SESSION" 2>/dev/null ||
        die "no wrapped session is running. Start one with: wrap <command>"
}

rand_range() {
    local min=$1 max=$2
    if ((min >= max)); then
        echo "$min"
        return
    fi
    echo $((min + RANDOM % (max - min + 1)))
}

sleep_ms() {
    local ms=$1
    sleep "$((ms / 1000)).$(printf '%03d' $((ms % 1000)))"
}

cmd_wrap() {
    if [[ "${1:-}" == "--kill" ]]; then
        require_session
        tmux kill-session -t "$SESSION"
        return
    fi
    (($# >= 1)) || die "usage: wrap <command...> | wrap --kill"
    if tmux has-session -t "$SESSION" 2>/dev/null; then
        die "a wrapped session is already running. Read it with wrap-read or stop it with: wrap --kill"
    fi
    tmux new-session -d -s "$SESSION" "$(printf '%q ' "$@")"
}

cmd_type() {
    local enter=0 delay_min=25 delay_max=120 words=()
    while (($# > 0)); do
        case "$1" in
        --enter) enter=1 ;;
        --delay-min-ms)
            delay_min="${2:?missing value for --delay-min-ms}"
            shift
            ;;
        --delay-max-ms)
            delay_max="${2:?missing value for --delay-max-ms}"
            shift
            ;;
        --)
            shift
            words+=("$@")
            break
            ;;
        *) words+=("$1") ;;
        esac
        shift
    done
    ((${#words[@]} >= 1)) ||
        die "usage: wrap-type [--enter] [--delay-min-ms N] [--delay-max-ms N] <text...>"
    ((delay_min <= delay_max)) ||
        die "--delay-min-ms must be less than or equal to --delay-max-ms"
    require_session

    local text="${words[*]}" ch ms i
    for ((i = 0; i < ${#text}; i++)); do
        ch="${text:i:1}"
        ms=$(rand_range "$delay_min" "$delay_max")
        case "$ch" in
        ' ') ((ms += $(rand_range 10 55))) ;;
        [.,\;:?!]) ((ms += $(rand_range 90 260))) ;;
        esac
        # A bare ; argument would end the tmux command sequence.
        [[ "$ch" == ';' ]] && ch='\;'
        tmux send-keys -t "$SESSION" -l -- "$ch"
        sleep_ms "$ms"
    done
    if ((enter)); then
        tmux send-keys -t "$SESSION" Enter
    fi
}

cmd_key() {
    (($# == 1)) || die "usage: wrap-key <key>   (tmux key name, e.g. Enter, Escape, BSpace, C-c)"
    require_session
    tmux send-keys -t "$SESSION" "$1"
}

cmd_read() {
    local lines=""
    while (($# > 0)); do
        case "$1" in
        --lines)
            lines="${2:?missing value for --lines}"
            shift
            ;;
        *) die "usage: wrap-read [--lines N]" ;;
        esac
        shift
    done
    require_session
    if [[ -n "$lines" ]]; then
        tmux capture-pane -p -t "$SESSION" -S "-$lines"
    else
        tmux capture-pane -p -t "$SESSION"
    fi
}

case "$(basename "$0")" in
wrap) cmd_wrap "$@" ;;
wrap-type) cmd_type "$@" ;;
wrap-key) cmd_key "$@" ;;
wrap-read) cmd_read "$@" ;;
*) die "unknown command name: $(basename "$0")" ;;
esac
