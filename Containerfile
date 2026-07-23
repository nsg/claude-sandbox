FROM docker.io/ubuntu:24.04 AS builder

RUN apt-get update && apt-get install -y curl unzip

# Starship
RUN curl -fsSL https://github.com/starship/starship/releases/latest/download/starship-x86_64-unknown-linux-musl.tar.gz | tar -xzf - -C /usr/local/bin

# Zola
RUN curl -fsSL -L https://github.com/getzola/zola/releases/download/v0.22.1/zola-v0.22.1-x86_64-unknown-linux-gnu.tar.gz | tar -xzf - -C /usr/local/bin zola

# Claude CLI
RUN curl -fsSL https://claude.ai/install.sh | bash

FROM docker.io/ubuntu:24.04

# Base packages
RUN apt-get update && apt-get upgrade -y && \
    apt-get install -y \
        curl \
        git \
        build-essential \
        pkg-config \
        libssl-dev \
        unzip \
        tree \
        patchutils \
        jq \
        yq \
        ruby \
        ffmpeg \
        openssh-server \
        rustup \
        shellcheck \
        bubblewrap \
        alsa-utils \
        libasound2-plugins \
        tmux \
        xvfb \
        openbox \
        dbus-x11 \
        xdotool \
        wmctrl \
        x11-utils \
        scrot \
        xterm \
        mesa-utils \
        vulkan-tools \
    && rm -rf /var/lib/apt/lists/*

# Route ALSA default device to PulseAudio so arecord/aplay work without a
# hardware sound card (otherwise ALSA spams "cannot find card '0'" errors)
RUN printf 'pcm.!default pulse\nctl.!default pulse\n' > /etc/asound.conf

# Rust toolchain
RUN rustup default stable
RUN rustup target add wasm32-unknown-unknown
RUN cargo install cargo-audit trunk

# Make cargo binaries available in all login shells (e.g. SSH sessions)
RUN echo 'export PATH="$HOME/.cargo/bin:$PATH"' > /etc/profile.d/cargo.sh

# Node.js 24 (Ubuntu 24.04 ships Node 18, which lacks global `crypto` —
# breaks t3code's crypto.randomUUID() call)
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - && \
    apt-get install -y nodejs && \
    rm -rf /var/lib/apt/lists/* && \
    node -v && npm -v

# Google Chrome for Playwright MCP
RUN curl -fsSL https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb \
        -o /tmp/google-chrome-stable_current_amd64.deb && \
    apt-get update && \
    apt-get install -y --no-install-recommends /tmp/google-chrome-stable_current_amd64.deb && \
    rm -f /tmp/google-chrome-stable_current_amd64.deb && \
    rm -rf /var/lib/apt/lists/*

# Copy binaries from builder
COPY --from=builder /usr/local/bin/starship /usr/local/bin/
COPY --from=builder /usr/local/bin/zola /usr/local/bin/
COPY --from=builder /root/.local/bin/claude /root/.local/bin/

# Install Playwright MCP server without downloading bundled browsers.
RUN PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 npm install -g @playwright/mcp

# OpenAI Codex CLI
RUN npm install -g @openai/codex

# t3code — web GUI for coding agents (https://github.com/pingdotgg/t3code)
RUN npm install -g t3

# opencode — TUI coding agent (https://opencode.ai)
RUN npm install -g opencode-ai

# Set PATH for all shells
ENV PATH="/root/.local/bin:$PATH"

# Configure starship in bashrc
RUN echo 'eval "$(starship init bash)"' >> /root/.bashrc

# Starship config
RUN mkdir -p /root/.config
COPY config/starship.toml /root/.config/starship.toml

# gh CLI proxy client (talks to host-side proxy via Unix socket)
COPY config/gh-proxy-client.js /usr/local/bin/gh
RUN chmod +x /usr/local/bin/gh

# Clipboard image proxy client (talks to host-side proxy via Unix socket)
COPY config/clipboard-proxy-client.js /usr/local/bin/xclip
RUN chmod +x /usr/local/bin/xclip
RUN ln -s /usr/local/bin/xclip /usr/local/bin/wl-paste

# SSH proxy client (talks to host-side proxy via Unix socket)
COPY config/ssh-proxy-client.js /usr/local/bin/ssh
RUN chmod +x /usr/local/bin/ssh

# git push bridge (talks to host-side proxy via Unix socket; enabled with --allow-push)
COPY config/git-proxy-client.js /usr/local/bin/git-proxy-client
COPY config/git-wrapper.sh /usr/local/bin/git
RUN chmod +x /usr/local/bin/git-proxy-client /usr/local/bin/git

# t3code instance launcher
COPY config/t3code-register.sh /usr/local/bin/t3code-register
COPY config/t3code-pair-admin.js /usr/local/lib/t3code-pair-admin.js
RUN chmod +x /usr/local/bin/t3code-register /usr/local/lib/t3code-pair-admin.js

# Virtual X display (Xvfb + openbox) for GUI app testing
COPY config/start-display.sh /usr/local/bin/start-display
RUN chmod +x /usr/local/bin/start-display && \
    echo '[ -f /run/claude-display.env ] && . /run/claude-display.env' > /etc/profile.d/claude-display.sh

# Managed configs (merged at runtime by entrypoint)
COPY config/mcp.json /etc/claude/mcp.json
COPY config/codex.toml /etc/codex/config.toml
COPY config/opencode.json /etc/opencode/opencode.json
COPY config/AGENTS.md /etc/AGENTS.md

# Entrypoint script for runtime configuration
COPY config/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

# Symlink ubuntu user's .claude and .codex to root's
RUN rm -rf /home/ubuntu/.claude && ln -s /root/.claude /home/ubuntu/.claude
RUN rm -rf /home/ubuntu/.codex && ln -s /root/.codex /home/ubuntu/.codex

WORKDIR /workspace

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]
