# ===========================================================================
# Dyson — multi-stage Docker build.
#
# Stage 1: Build the Rust binary in a full toolchain image.
# Stage 2: Copy into a slim runtime with Whisper + FFmpeg.
#
# Usage:
#   docker build -t dyson .
#   docker run --env-file .env -v ./dyson.json:/etc/dyson/dyson.json:ro dyson --config /etc/dyson/dyson.json
# ===========================================================================

# ---------------------------------------------------------------------------
# Build stage
# ---------------------------------------------------------------------------
FROM rust:1.85-bookworm AS builder

WORKDIR /build

# Cache dependencies: copy manifests first, build a dummy, then copy source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release 2>/dev/null || true \
    && rm -rf src

# Copy the real source and build.
COPY src/ src/
COPY tests/ tests/
RUN cargo build --release

# ---------------------------------------------------------------------------
# Runtime stage
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    python3 \
    python3-pip \
    python3-venv \
    ffmpeg \
    && rm -rf /var/lib/apt/lists/*

# Install Whisper in a virtual environment.
RUN python3 -m venv /opt/whisper \
    && /opt/whisper/bin/pip install --no-cache-dir openai-whisper \
    && ln -s /opt/whisper/bin/whisper /usr/local/bin/whisper

# Copy the binary from the build stage.
COPY --from=builder /build/target/release/dyson /usr/local/bin/dyson

# Create data directories.
RUN mkdir -p /data/chats /data/workspace

# Default config location.
VOLUME ["/etc/dyson", "/data"]

ENTRYPOINT ["dyson"]
CMD ["--config", "/etc/dyson/dyson.json"]
