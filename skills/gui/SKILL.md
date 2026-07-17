---
name: gui
description: Run and test GUI applications on the container's virtual X display (screenshots, clicking, typing)
---

# GUI App Testing

The container runs a virtual X display: Xvfb on `:99` with the openbox window
manager and a session D-Bus. `DISPLAY` is already set for all shells, so GUI
apps (GTK, Qt, X11, headed Chrome) launch directly — no physical screen needed.

If `DISPLAY` is unset or apps fail to connect, run `start-display` (idempotent)
and source `/run/claude-display.env`.

## Core loop: see → act → verify

1. Launch the app in the background: `some-app &` then `sleep` briefly.
2. Screenshot: `scrot /tmp/shot.png` (full screen) or `scrot -u /tmp/shot.png`
   (focused window), then view the PNG with the Read tool.
3. Act with xdotool using coordinates from the screenshot.
4. Screenshot again to verify the result. Never assume an action worked.

## xdotool cheat sheet

- `xdotool mousemove X Y click 1` — move and left-click (3 = right, 4/5 = scroll)
- `xdotool type --delay 50 'text'` — type into the focused window
- `xdotool key Return` / `key ctrl+s` / `key alt+F4` — key presses
- `xdotool search --name 'Title' windowactivate --sync` — focus a window by title
- `xdotool search --class appname getwindowgeometry` — window position/size
- `xdotool getactivewindow getwindowname` — verify which window has focus
- `wmctrl -l` — list all windows

## Tips

- Screen is 1280x800 by default (override with `XVFB_SCREEN` before
  `start-display`, e.g. `1920x1080x24`).
- Headed Chrome works: `google-chrome --no-sandbox URL` — useful when a test
  needs a real browser window instead of the Playwright MCP headless instance.
- Focus first, then type: activate the target window before `xdotool type`.
- For animations or timing bugs, record with
  `ffmpeg -f x11grab -i :99 -t 5 out.mp4` and extract frames.
- Kill test apps by PID (`kill %1` or `pkill -x appname`). Do not use
  `pkill -f` with a pattern that appears in your own command line — it kills
  your own shell.
