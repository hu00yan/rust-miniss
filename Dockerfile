# Development environment for rust-miniss
FROM rust:1.83-slim

# Install dependencies for io-uring and development tools
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    liburing-dev \
    htop \
    strace \
    linux-perf \
    && rm -rf /var/lib/apt/lists/*

# Set up working directory
WORKDIR /workspace/rust-miniss

# Install cargo tools (skip if fails - optional development tools)
RUN cargo install cargo-watch || echo "cargo-watch installation failed, skipping..."
RUN cargo install cargo-expand || echo "cargo-expand installation failed, skipping..."

# Copy project files
COPY . .

# Build dependencies (this layer will be cached)
RUN cargo fetch

# Set up shell
ENV SHELL=/bin/bash
RUN echo 'alias ll="ls -la"' >> ~/.bashrc
RUN echo 'alias c="cargo"' >> ~/.bashrc
RUN echo 'alias cw="cargo watch -x check -x test -x run"' >> ~/.bashrc

CMD ["/bin/bash"]
