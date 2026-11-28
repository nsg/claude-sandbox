#!/bin/bash
# Provision a Blender + official Blender Lab MCP server inside the guest.
# Runs as root in the VM, fed by `blender-vm up` over `vm pipe`. Idempotent:
# re-running upgrades or repairs whatever is missing, then restarts Blender.
set -euo pipefail

# --- Pinned versions (bump together; every download is checksum-verified) ---
BLENDER_VERSION=5.3.0
BLENDER_SERIES=5.3
BLENDER_SHA256=dacb8264197a3f7fda6001285a5a506fc97ee943881261c2c3bd63c882f22e05
# Blender Lab MCP add-on (Blender extension) and server, tag v1.0.3.
MCP_VERSION=1.0.3
MCP_ADDON_SHA256=a7a9da816192502e5a0a202a396444e266b47d8fc4f74ad4698048bd43040707
MCP_COMMIT=2cea8d566dde07fbac28a61d698909d69724e853
# -----------------------------------------------------------------------------

BLENDER_DIR=/opt/blender-$BLENDER_VERSION
MCP_VENV=/opt/blender-mcp
CACHE=/var/cache/blender-vm
LIB=/usr/local/lib/blender-vm
export DEBIAN_FRONTEND=noninteractive

log() { printf '[provision] %s\n' "$*" >&2; }

fetch() { # fetch URL DEST SHA256
  if [ -f "$2" ] && echo "$3  $2" | sha256sum -c --status; then return; fi
  log "downloading $1"
  curl -fsSL --retry 3 -o "$2.part" "$1"
  echo "$3  $2.part" | sha256sum -c --quiet
  mv "$2.part" "$2"
}

log "installing packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  ca-certificates curl xz-utils git python3-venv \
  xserver-xorg-core xserver-xorg-input-libinput xinit x11-xserver-utils openbox \
  libgl1 libegl1 libgl1-mesa-dri libglx-mesa0 mesa-vulkan-drivers libvulkan1 \
  libx11-6 libx11-xcb1 libxi6 libxxf86vm1 libxfixes3 libxrender1 libxcursor1 \
  libxinerama1 libxrandr2 libxkbcommon0 libxkbcommon-x11-0 libsm6 libice6 \
  libwayland-client0 libwayland-cursor0 libwayland-egl1 libdecor-0-0 \
  libfontconfig1 libfreetype6 >/dev/null

mkdir -p "$CACHE" "$LIB" /root/work

if [ ! -x "$BLENDER_DIR/blender" ]; then
  tarball=$CACHE/blender-$BLENDER_VERSION-linux-x64.tar.xz
  fetch "https://download.blender.org/release/Blender$BLENDER_SERIES/blender-$BLENDER_VERSION-linux-x64.tar.xz" \
    "$tarball" "$BLENDER_SHA256"
  log "unpacking Blender $BLENDER_VERSION"
  rm -rf "$BLENDER_DIR.part"
  mkdir -p "$BLENDER_DIR.part"
  tar -xJf "$tarball" -C "$BLENDER_DIR.part" --strip-components=1
  mv "$BLENDER_DIR.part" "$BLENDER_DIR"
  rm -f "$tarball"
fi
ln -sfn "$BLENDER_DIR" /opt/blender
ln -sfn /opt/blender/blender /usr/local/bin/blender

addon_zip=$CACHE/mcp-$MCP_VERSION.zip
fetch "https://projects.blender.org/lab/blender_mcp/releases/download/v$MCP_VERSION/mcp-$MCP_VERSION.zip" \
  "$addon_zip" "$MCP_ADDON_SHA256"
log "installing and enabling the MCP add-on"
blender --background --factory-startup --online-mode \
  --command extension install-file --repo user_default --enable "$addon_zip" >&2
# No splash screen: it would cover the window in screenshots.
blender --background --online-mode --python-expr \
  'import bpy; bpy.context.preferences.view.show_splash = False; bpy.ops.wm.save_userpref()' >/dev/null

if [ "$("$MCP_VENV/bin/python" -c 'import importlib.metadata as m; print(m.version("blender-mcp"))' 2>/dev/null)" != "$MCP_VERSION" ]; then
  log "installing the MCP server (blender_mcp @ $MCP_COMMIT)"
  rm -rf "$MCP_VENV"
  python3 -m venv "$MCP_VENV"
  "$MCP_VENV/bin/pip" install -q --disable-pip-version-check \
    "blender-mcp @ git+https://projects.blender.org/lab/blender_mcp.git@$MCP_COMMIT#subdirectory=mcp"
fi

cat > /usr/local/bin/blender-mcp-stdio <<'EOF'
#!/bin/sh
# MCP server on stdio, talking to the add-on in the running Blender.
export BLENDER_PATH=/opt/blender/blender BLENDER_MCP_HOST=127.0.0.1 BLENDER_MCP_PORT=9876
exec /opt/blender-mcp/bin/blender-mcp "$@"
EOF
chmod 755 /usr/local/bin/blender-mcp-stdio

cat > "$LIB/session" <<'EOF'
#!/bin/sh
# X session: a window manager plus Blender, maximised, add-on auto-starting.
xset s off -dpms
openbox &
cd /root/work
exec blender --online-mode -p 0 0 1280 800
EOF
chmod 755 "$LIB/session"

cat > /etc/systemd/system/blender-gui.service <<EOF
[Unit]
Description=Blender on the VM display, with the MCP add-on listening on 127.0.0.1:9876
After=systemd-user-sessions.service
Conflicts=getty@tty7.service

[Service]
Environment=HOME=/root XDG_RUNTIME_DIR=/run/blender-gui
RuntimeDirectory=blender-gui
RuntimeDirectoryMode=0700
WorkingDirectory=/root/work
ExecStart=/usr/bin/xinit $LIB/session -- /usr/bin/Xorg :0 vt7 -nolisten tcp -keeptty -noreset
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable blender-gui.service >/dev/null 2>&1
systemctl restart blender-gui.service

log "waiting for the MCP add-on to listen on 127.0.0.1:9876"
for _ in $(seq 1 90); do
  if (exec 3<>/dev/tcp/127.0.0.1/9876) 2>/dev/null; then
    # blender-vm compares this with the local script to skip needless re-runs.
    echo "${BLENDER_VM_REV:-unknown}" > /etc/blender-vm.rev
    log "ready: Blender $BLENDER_VERSION, MCP $MCP_VERSION"
    exit 0
  fi
  sleep 2
done
log "Blender did not start listening; recent log:"
journalctl -u blender-gui.service -n 60 --no-pager >&2
exit 1
