#!/bin/bash

# Configure git at runtime if environment variables are set
if [ -n "$GIT_USER_NAME" ]; then
    git config --global user.name "$GIT_USER_NAME"
fi

if [ -n "$GIT_USER_EMAIL" ]; then
    git config --global user.email "$GIT_USER_EMAIL"
fi

# Use SSH proxy for git remote operations (if proxy socket exists)
if [ -S /run/claude-sandbox/ssh-proxy.sock ] || [ -S /workspace/.claude-sandbox/ssh-proxy.sock ]; then
    git config --global core.sshCommand "/usr/local/bin/ssh"
fi

# Merge image MCP config into project-level config
if [ -f /etc/claude/mcp.json ]; then
    MCP_TARGET=/workspace/.mcp.json
    if [ -f "$MCP_TARGET" ]; then
        # Merge: image config wins over project config for shared servers
        jq -s '.[0] * .[1]' "$MCP_TARGET" /etc/claude/mcp.json > "$MCP_TARGET.tmp" \
            && mv "$MCP_TARGET.tmp" "$MCP_TARGET"
    else
        cp /etc/claude/mcp.json "$MCP_TARGET"
    fi
fi

# Managed configs and instructions, deployed to each harness home.

# Merge a managed TOML file into a user config inside # MANAGED markers,
# preserving user-owned content outside the block.
TOML_MANAGED_START="# MANAGED START"
TOML_MANAGED_END="# MANAGED END"
merge_managed_toml() {
    local source_file="$1"
    local target_file="$2"
    local user_content=""
    local managed_block=""

    mkdir -p "$(dirname "$target_file")"

    if [ -f "$target_file" ]; then
        user_content=$(sed "/$TOML_MANAGED_START/,/$TOML_MANAGED_END/d" "$target_file")
    fi

    managed_block="$TOML_MANAGED_START
$(cat "$source_file")
$TOML_MANAGED_END"
    printf '%s\n' "$managed_block" > "$target_file"
    if [ -n "$user_content" ]; then
        printf '%s' "$user_content" >> "$target_file"
    fi
}

# Merge a managed JSON file into a user config; managed keys win.
merge_managed_json() {
    local source_file="$1"
    local target_file="$2"

    mkdir -p "$(dirname "$target_file")"
    if [ -f "$target_file" ]; then
        jq -s '.[0] * .[1]' "$target_file" "$source_file" > "$target_file.tmp" \
            && mv "$target_file.tmp" "$target_file"
    else
        cp "$source_file" "$target_file"
    fi
}

# Merge managed AGENTS.md from image into a harness instructions file.
# The managed part is the H1 section from the source file (# Global
# Instructions): it is replaced on every start, up to the next H1 or EOF.
# Users can add their own H1 sections below; those are preserved.
# Each harness gets the shared base plus an optional overlay
# (/etc/AGENTS.<harness>.md). Overlays must contain only ## sections — no
# H1 — so their content stays inside the managed H1 section (enforced in CI).
MANAGED_SOURCE=/etc/AGENTS.md
merge_managed_instructions() {
    local harness="$1"
    local target_file="$2"
    local overlay="/etc/AGENTS.$harness.md"
    local managed_h1=""
    local managed_block=""
    local user_content=""

    [ -f "$MANAGED_SOURCE" ] || return 0
    managed_h1=$(head -n 1 "$MANAGED_SOURCE")
    managed_block=$(cat "$MANAGED_SOURCE")
    if [ -s "$overlay" ]; then
        managed_block="$managed_block

$(cat "$overlay")"
    fi

    mkdir -p "$(dirname "$target_file")"

    if [ -f "$target_file" ]; then
        user_content=$(awk -v h1="$managed_h1" '
            # Clean up legacy <!-- MANAGED --> marker blocks
            $0 == "<!-- MANAGED START -->" { legacy = 1; next }
            legacy { if ($0 == "<!-- MANAGED END -->") legacy = 0; next }
            # Drop the managed H1 section (until the next H1)
            $0 == h1 { managed = 1; next }
            managed { if ($0 ~ /^# /) managed = 0; else next }
            # Skip blank lines before the first kept line
            !started && $0 == "" { next }
            { started = 1; print }
        ' "$target_file")
    fi

    printf '%s\n' "$managed_block" > "$target_file"
    if [ -n "$user_content" ]; then
        printf '\n%s\n' "$user_content" >> "$target_file"
    fi
}

# Symlink Claude's auto-memory into .claude-sandbox so it's per-project
# (all containers mount at /workspace, so the slug is always "-workspace")
MEMORY_TARGET=/workspace/.claude-sandbox/memory
link_claude_memory() {
    local memory_link="$1/projects/-workspace/memory"

    if [ ! -L "$memory_link" ]; then
        mkdir -p "$(dirname "$memory_link")"
        if [ -d "$memory_link" ] && [ ! -e "$MEMORY_TARGET" ]; then
            # Migrate existing memory into the project folder
            mkdir -p "$(dirname "$MEMORY_TARGET")"
            mv "$memory_link" "$MEMORY_TARGET"
        elif [ -d "$memory_link" ]; then
            # The project already has memory; keep this copy aside
            mv "$memory_link" "$memory_link.pre-sandbox.$(date +%s)"
        fi
        ln -s "$MEMORY_TARGET" "$memory_link"
    fi
    # Ensure the target directory exists (symlink may point to a not-yet-created path)
    mkdir -p "$MEMORY_TARGET"
}

deploy_claude_home() {
    merge_managed_instructions claude "$1/CLAUDE.md"
    link_claude_memory "$1"
}

deploy_codex_home() {
    if [ -f /etc/codex/config.toml ]; then
        merge_managed_toml /etc/codex/config.toml "$1/config.toml"
    fi
    merge_managed_instructions codex "$1/AGENTS.md"
}

deploy_opencode_home() {
    if [ -f /etc/opencode/opencode.json ]; then
        merge_managed_json /etc/opencode/opencode.json "$1/opencode.json"
    fi
    merge_managed_instructions opencode "$1/AGENTS.md"
}

deploy_claude_home "$HOME/.claude"
deploy_codex_home "$HOME/.codex"
deploy_opencode_home "$HOME/.config/opencode"

# Choose Buildah's storage driver. Overlay is far smaller and faster than vfs
# but needs a real filesystem under /var/lib/containers, which the image's
# volume normally provides; vfs works anywhere. Non-fatal: image builds are
# optional.
if command -v buildah >/dev/null 2>&1 && [ ! -e /etc/containers/storage.conf ]; then
    if buildah --storage-driver overlay info >/dev/null 2>&1; then
        storage_driver=overlay
    else
        storage_driver=vfs
        rm -rf /var/lib/containers/storage /run/containers/storage
    fi
    printf '[storage]\ndriver = "%s"\nrunroot = "%s"\ngraphroot = "%s"\n' \
        "$storage_driver" /run/containers/storage /var/lib/containers/storage \
        > /etc/containers/storage.conf 2>/dev/null || true
fi

# Start the virtual X display (Xvfb + openbox) and inherit its environment
# (DISPLAY, DBUS_SESSION_BUS_ADDRESS). Non-fatal: GUI support is optional.
if /usr/local/bin/start-display; then
    # shellcheck source=/dev/null
    [ -f /run/claude-display.env ] && source /run/claude-display.env
fi

# Execute the command passed to the container
exec "$@"
