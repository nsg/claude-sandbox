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
        tree \
        patchutils \
        jq \
        nodejs \
        npm \
        rustup \
    && rm -rf /var/lib/apt/lists/*

# Rust toolchain
RUN rustup default stable

# Copy binaries from builder
COPY --from=builder /usr/local/bin/starship /usr/local/bin/
COPY --from=builder /usr/local/bin/zola /usr/local/bin/
COPY --from=builder /root/.local/bin/claude /root/.local/bin/

# Set PATH for all shells
ENV PATH="/root/.local/bin:$PATH"

# Configure starship in bashrc
RUN echo 'eval "$(starship init bash)"' >> /root/.bashrc

# Starship config
RUN mkdir -p /root/.config
COPY config/starship.toml /root/.config/starship.toml

# Entrypoint script for runtime configuration
COPY config/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

WORKDIR /workspace

EXPOSE 3456

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
CMD ["/bin/bash"]
