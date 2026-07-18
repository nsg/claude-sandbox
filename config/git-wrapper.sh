#!/bin/sh
# git shim: bridge exactly `git push` / `git push --tags` to the host-side
# push proxy when it is enabled (claude-sandbox --allow-push). Everything
# else execs the real git in-place, so behavior is byte-for-byte identical.

SOCKET=/workspace/.claude-sandbox/git-proxy.sock

if [ "$1" = "push" ]; then
    if [ -S "$SOCKET" ]; then
        case "$*" in
            push|"push --tags")
                exec /usr/local/bin/git-proxy-client "$@"
                ;;
        esac
    fi
    /usr/bin/git "$@"
    status=$?
    if [ "$status" -ne 0 ]; then
        if [ -S "$SOCKET" ]; then
            echo "hint: only plain 'git push' and 'git push --tags' are bridged to the host" >&2
        else
            echo "hint: pushes from the sandbox are disabled; relaunch with 'claude-sandbox --allow-push' to enable them" >&2
        fi
    fi
    exit "$status"
fi

exec /usr/bin/git "$@"
