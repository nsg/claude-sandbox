#!/bin/bash

# Configure git at runtime if environment variables are set
if [ -n "$GIT_USER_NAME" ]; then
    git config --global user.name "$GIT_USER_NAME"
fi

if [ -n "$GIT_USER_EMAIL" ]; then
    git config --global user.email "$GIT_USER_EMAIL"
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

# Merge managed CLAUDE.md from image, preserving user additions
CLAUDE_MD="$HOME/.claude/CLAUDE.md"
MANAGED_START="<!-- MANAGED START -->"
MANAGED_END="<!-- MANAGED END -->"
if [ -f /etc/claude/CLAUDE.md ]; then
    mkdir -p "$(dirname "$CLAUDE_MD")"
    MANAGED_BLOCK="$MANAGED_START
$(cat /etc/claude/CLAUDE.md)
$MANAGED_END"
    if [ -f "$CLAUDE_MD" ]; then
        USER_CONTENT=$(sed "/$MANAGED_START/,/$MANAGED_END/d" "$CLAUDE_MD")
    else
        USER_CONTENT=""
    fi
    printf '%s\n' "$MANAGED_BLOCK" > "$CLAUDE_MD"
    if [ -n "$USER_CONTENT" ]; then
        printf '%s' "$USER_CONTENT" >> "$CLAUDE_MD"
    fi
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

# Execute the command passed to the container
exec "$@"
