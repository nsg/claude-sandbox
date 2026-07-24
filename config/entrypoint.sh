#!/bin/bash

# Configure git at runtime if environment variables are set
if [ -n "$GIT_USER_NAME" ]; then
    git config --global user.name "$GIT_USER_NAME"
fi

if [ -n "$GIT_USER_EMAIL" ]; then
    git config --global user.email "$GIT_USER_EMAIL"
fi

# Use SSH proxy for git remote operations (if proxy socket exists)
if [ -S /workspace/.claude-sandbox/ssh-proxy.sock ]; then
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

# Merge managed opencode config into user-level opencode.json
if [ -f /etc/opencode/opencode.json ]; then
    OPENCODE_TARGET="$HOME/.config/opencode/opencode.json"
    mkdir -p "$(dirname "$OPENCODE_TARGET")"
    if [ -f "$OPENCODE_TARGET" ]; then
        # Merge: image config wins over user config for shared keys (e.g. mcp.playwright)
        jq -s '.[0] * .[1]' "$OPENCODE_TARGET" /etc/opencode/opencode.json > "$OPENCODE_TARGET.tmp" \
            && mv "$OPENCODE_TARGET.tmp" "$OPENCODE_TARGET"
    else
        cp /etc/opencode/opencode.json "$OPENCODE_TARGET"
    fi
fi

# Merge managed Codex config into user-level config.toml
MANAGED_CODEX_CONFIG_SOURCE=/etc/codex/config.toml
MANAGED_CODEX_CONFIG_TARGET="$HOME/.codex/config.toml"
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

if [ -f "$MANAGED_CODEX_CONFIG_SOURCE" ]; then
    merge_managed_toml "$MANAGED_CODEX_CONFIG_SOURCE" "$MANAGED_CODEX_CONFIG_TARGET"
fi

# Merge managed AGENTS.md from image into user-level Claude and Codex files.
# The managed part is the H1 section from the source file (# Global
# Instructions): it is replaced on every start, up to the next H1 or EOF.
# Users can add their own H1 sections below; those are preserved.
# Each harness gets the shared base plus an optional overlay
# (/etc/AGENTS.<harness>.md). Overlays must contain only ## sections — no
# H1 — so their content stays inside the managed H1 section (enforced in CI).
MANAGED_SOURCE=/etc/AGENTS.md
CLAUDE_MD="$HOME/.claude/CLAUDE.md"
CODEX_AGENTS_MD="$HOME/.codex/AGENTS.md"
OPENCODE_AGENTS_MD="$HOME/.config/opencode/AGENTS.md"
merge_managed_file() {
    local target_file="$1"
    local user_content=""

    mkdir -p "$(dirname "$target_file")"

    if [ -f "$target_file" ]; then
        user_content=$(awk -v h1="$MANAGED_H1" '
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

    printf '%s\n' "$MANAGED_BLOCK" > "$target_file"
    if [ -n "$user_content" ]; then
        printf '\n%s\n' "$user_content" >> "$target_file"
    fi
}

compose_managed_block() {
    local overlay="/etc/AGENTS.$1.md"
    MANAGED_BLOCK=$(cat "$MANAGED_SOURCE")
    if [ -s "$overlay" ]; then
        MANAGED_BLOCK="$MANAGED_BLOCK

$(cat "$overlay")"
    fi
}

if [ -f "$MANAGED_SOURCE" ]; then
    MANAGED_H1=$(head -n 1 "$MANAGED_SOURCE")
    compose_managed_block claude
    merge_managed_file "$CLAUDE_MD"
    compose_managed_block codex
    merge_managed_file "$CODEX_AGENTS_MD"
    compose_managed_block opencode
    merge_managed_file "$OPENCODE_AGENTS_MD"
fi

# Symlink auto-memory into .claude-sandbox so it's per-project
# (all containers mount at /workspace, so the slug is always "-workspace")
MEMORY_LINK="$HOME/.claude/projects/-workspace/memory"
MEMORY_TARGET=/workspace/.claude-sandbox/memory
if [ ! -L "$MEMORY_LINK" ]; then
    mkdir -p "$(dirname "$MEMORY_LINK")"
    if [ -d "$MEMORY_LINK" ]; then
        # Migrate existing memory into the project folder
        mkdir -p "$(dirname "$MEMORY_TARGET")"
        mv "$MEMORY_LINK" "$MEMORY_TARGET"
    else
        mkdir -p "$MEMORY_TARGET"
    fi
    ln -s "$MEMORY_TARGET" "$MEMORY_LINK"
fi
# Ensure the target directory exists (symlink may point to a not-yet-created path)
mkdir -p "$MEMORY_TARGET"

# Start SSH server if authorized key is provided
if [ -n "$SSH_AUTHORIZED_KEY" ]; then
    mkdir -p /root/.ssh
    chmod 700 /root/.ssh
    echo "$SSH_AUTHORIZED_KEY" > /root/.ssh/authorized_keys
    chmod 600 /root/.ssh/authorized_keys

    SSHD_JSON="/workspace/.claude-sandbox/sshd.json"

    # Restore or generate host keys
    if [ -f "$SSHD_JSON" ] && jq -e '.host_keys' "$SSHD_JSON" > /dev/null 2>&1; then
        # Write persisted host keys to /etc/ssh/
        for keyname in $(jq -r '.host_keys | keys[]' "$SSHD_JSON"); do
            jq -r --arg k "$keyname" '.host_keys[$k]' "$SSHD_JSON" > "/etc/ssh/$keyname"
            # Private keys need strict permissions
            case "$keyname" in
                *.pub) chmod 644 "/etc/ssh/$keyname" ;;
                *)     chmod 600 "/etc/ssh/$keyname" ;;
            esac
        done
    else
        # First run: generate host keys and persist them
        ssh-keygen -A

        HOST_KEYS_JSON="{}"
        for keyfile in /etc/ssh/ssh_host_*; do
            keyname="$(basename "$keyfile")"
            content="$(cat "$keyfile")"
            HOST_KEYS_JSON=$(echo "$HOST_KEYS_JSON" | jq --arg k "$keyname" --arg v "$content" '. + {($k): $v}')
        done

        mkdir -p "$(dirname "$SSHD_JSON")"
        if [ -f "$SSHD_JSON" ]; then
            jq --argjson hk "$HOST_KEYS_JSON" '. + {host_keys: $hk}' "$SSHD_JSON" > "$SSHD_JSON.tmp" \
                && mv "$SSHD_JSON.tmp" "$SSHD_JSON"
        else
            echo "{}" | jq --argjson hk "$HOST_KEYS_JSON" '{host_keys: $hk}' > "$SSHD_JSON"
        fi
    fi

    # Configure sshd: key-only auth, no password, no PAM
    mkdir -p /run/sshd
    cat > /etc/ssh/sshd_config.d/sandbox.conf <<SSHEOF
PermitRootLogin prohibit-password
PasswordAuthentication no
UsePAM no
SSHEOF

    /usr/sbin/sshd
fi

# Start the virtual X display (Xvfb + openbox) and inherit its environment
# (DISPLAY, DBUS_SESSION_BUS_ADDRESS). Non-fatal: GUI support is optional.
if /usr/local/bin/start-display; then
    # shellcheck source=/dev/null
    [ -f /run/claude-display.env ] && source /run/claude-display.env
fi

# Execute the command passed to the container
exec "$@"
