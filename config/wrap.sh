#!/bin/bash
# Wrapped tmux session helpers, installed as wrap, wrap-type, wrap-key and
# wrap-read (symlinks dispatch on the invocation name). The host-side
# `claude-sandbox wrap-*` commands forward here via podman exec, so this
# script is the single implementation of the wrap behaviour.
#
# Sessions are named with --session; the default name must match
# WRAP_TMUX_SESSION in claude-sandbox/src/main.rs.
set -euo pipefail

DEFAULT_SESSION="${CLAUDE_WRAP_SESSION:-claude-sandbox}"
SESSION=""

die() {
    echo "Error: $*" >&2
    exit 1
}

list_sessions() {
    tmux list-sessions -F '#{session_name}' 2>/dev/null || true
}

session_exists() {
    tmux has-session -t "=$1" 2>/dev/null
}

# Pick the target session: an explicit --session name must exist; without
# one, use the only running session, or fail if there are none or several.
resolve_session() {
    local requested="${1:-${CLAUDE_WRAP_SESSION:-}}"
    if [[ -n "$requested" ]]; then
        session_exists "$requested" ||
            die "no wrapped session named '$requested'. List sessions with: wrap --list"
        SESSION="$requested"
        return
    fi
    local sessions=()
    mapfile -t sessions < <(list_sessions)
    case ${#sessions[@]} in
    0) die "no wrapped session is running. Start one with: wrap <command>" ;;
    1) SESSION="${sessions[0]}" ;;
    *) die "several wrapped sessions are running (${sessions[*]}). Pick one with --session <name>" ;;
    esac
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
    local name="" kill=0
    while (($# > 0)); do
        case "$1" in
        --session)
            name="${2:?missing value for --session}"
            shift
            ;;
        --kill) kill=1 ;;
        --list)
            list_sessions
            return
            ;;
        --)
            shift
            break
            ;;
        *) break ;;
        esac
        shift
    done
    if ((kill)); then
        resolve_session "$name"
        tmux kill-session -t "=$SESSION"
        return
    fi
    (($# >= 1)) ||
        die "usage: wrap [--session <name>] <command...> | wrap --kill [--session <name>] | wrap --list"
    SESSION="${name:-$DEFAULT_SESSION}"
    [[ "$SESSION" =~ ^[A-Za-z0-9_-]+$ ]] ||
        die "session names may only contain letters, digits, dash and underscore"
    if session_exists "$SESSION"; then
        die "a wrapped session named '$SESSION' is already running. Start another with --session <name>, or stop it with: wrap --kill --session $SESSION"
    fi
    tmux new-session -d -s "$SESSION" "$(printf '%q ' "$@")"
}

cmd_type() {
    local name="" enter=0 delay_min=25 delay_max=120 words=()
    while (($# > 0)); do
        case "$1" in
        --session)
            name="${2:?missing value for --session}"
            shift
            ;;
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
        die "usage: wrap-type [--session <name>] [--enter] [--delay-min-ms N] [--delay-max-ms N] <text...>"
    ((delay_min <= delay_max)) ||
        die "--delay-min-ms must be less than or equal to --delay-max-ms"
    resolve_session "$name"

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
        tmux send-keys -t "=$SESSION:" -l -- "$ch"
        sleep_ms "$ms"
    done
    if ((enter)); then
        tmux send-keys -t "=$SESSION:" Enter
    fi
}

cmd_key() {
    local name="" keys=()
    while (($# > 0)); do
        case "$1" in
        --session)
            name="${2:?missing value for --session}"
            shift
            ;;
        *) keys+=("$1") ;;
        esac
        shift
    done
    ((${#keys[@]} == 1)) ||
        die "usage: wrap-key [--session <name>] <key>   (tmux key name, e.g. Enter, Escape, BSpace, C-c)"
    resolve_session "$name"
    tmux send-keys -t "=$SESSION:" "${keys[0]}"
}

cmd_read() {
    local name="" lines=""
    while (($# > 0)); do
        case "$1" in
        --session)
            name="${2:?missing value for --session}"
            shift
            ;;
        --lines)
            lines="${2:?missing value for --lines}"
            shift
            ;;
        *) die "usage: wrap-read [--session <name>] [--lines N]" ;;
        esac
        shift
    done
    resolve_session "$name"
    if [[ -n "$lines" ]]; then
        tmux capture-pane -p -t "=$SESSION:" -S "-$lines"
    else
        tmux capture-pane -p -t "=$SESSION:"
    fi
}

case "$(basename "$0")" in
wrap) cmd_wrap "$@" ;;
wrap-type) cmd_type "$@" ;;
wrap-key) cmd_key "$@" ;;
wrap-read) cmd_read "$@" ;;
*) die "unknown command name: $(basename "$0")" ;;
esac
