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

# Execute the command passed to the container
exec "$@"
