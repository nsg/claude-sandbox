#!/bin/bash

# Start a virtual X display (Xvfb + openbox + session D-Bus) for GUI apps.
# Idempotent: safe to call multiple times; only starts what is not running.
#
# Chosen over a headless Wayland compositor (sway/wlrctl) because X11 tooling
# supports absolute pointer positioning and window lookup (xdotool, wmctrl),
# which a screenshot -> click-at-coordinates workflow depends on.
#
# Configuration:
#   XVFB_DISPLAY  display to use (default :99)
#   XVFB_SCREEN   virtual screen geometry (default 1280x800x24)
#
# Writes session environment (DISPLAY, DBUS_SESSION_BUS_ADDRESS) to
# /run/claude-display.env so login shells and the entrypoint can source it.

XVFB_DISPLAY="${XVFB_DISPLAY:-:99}"
XVFB_SCREEN="${XVFB_SCREEN:-1280x800x24}"
ENV_FILE=/run/claude-display.env
LOG_FILE=/tmp/claude-display.log

socket_path="/tmp/.X11-unix/X${XVFB_DISPLAY#:}"

if [ ! -S "$socket_path" ]; then
    Xvfb "$XVFB_DISPLAY" -screen 0 "$XVFB_SCREEN" -nolisten tcp >>"$LOG_FILE" 2>&1 &
    for _ in $(seq 1 50); do
        [ -S "$socket_path" ] && break
        sleep 0.1
    done
    if [ ! -S "$socket_path" ]; then
        echo "start-display: Xvfb failed to create $socket_path" >&2
        exit 1
    fi
fi

export DISPLAY="$XVFB_DISPLAY"

# The Vulkan loader (and some Wayland/GTK code paths) want XDG_RUNTIME_DIR
if [ -z "$XDG_RUNTIME_DIR" ]; then
    XDG_RUNTIME_DIR="/run/user/$(id -u)"
    export XDG_RUNTIME_DIR
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
fi

# Session D-Bus keeps GTK apps from warning/failing on the session bus
if [ -z "$DBUS_SESSION_BUS_ADDRESS" ] && [ -f "$ENV_FILE" ]; then
    # Reuse the bus from a previous invocation if it is still alive
    # shellcheck source=/dev/null
    source "$ENV_FILE"
    if [ -n "$DBUS_SESSION_BUS_PID" ] && ! kill -0 "$DBUS_SESSION_BUS_PID" 2>/dev/null; then
        unset DBUS_SESSION_BUS_ADDRESS DBUS_SESSION_BUS_PID
    fi
fi
if [ -z "$DBUS_SESSION_BUS_ADDRESS" ]; then
    eval "$(dbus-launch --sh-syntax)" >>"$LOG_FILE" 2>&1
fi

if ! pgrep -x openbox >/dev/null; then
    openbox >>"$LOG_FILE" 2>&1 &
fi

{
    echo "export DISPLAY=$DISPLAY"
    echo "export XDG_RUNTIME_DIR='$XDG_RUNTIME_DIR'"
    [ -n "$DBUS_SESSION_BUS_ADDRESS" ] && echo "export DBUS_SESSION_BUS_ADDRESS='$DBUS_SESSION_BUS_ADDRESS'"
    [ -n "$DBUS_SESSION_BUS_PID" ] && echo "export DBUS_SESSION_BUS_PID=$DBUS_SESSION_BUS_PID"
} > "$ENV_FILE"
