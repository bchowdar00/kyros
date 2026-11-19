#
# Kyros exchange server, for Azure Container Apps.
#
# Two stages: the `exchange` binary is built on the pinned Rust toolchain
# (rust-toolchain.toml → 1.93.0) and copied into a slim Debian runtime that
# carries only the binary and the data files the binary opens at run time.
#
# The server reads two directories relative to its working directory at
# startup (see crates/exchange/src/copy.rs and registry.rs):
#   - copy/signals/*.json         the human-readable signal copy
#   - definitions/<u>/signals/*.json   the signal definitions for the registry
# and writes registry.json into the working directory, so the working
# directory must be writable by the runtime user. The schemas/ and template
# JSON are include_str!'d into the binary at compile time and are not needed
# at run time.
#
# Build context is `server/`. Architecture-neutral: no platform is named, so
# `docker build` produces an image for the host's own architecture.

# ───────────────────────────── build ─────────────────────────────
FROM rust:1.93.0-bookworm AS build

WORKDIR /app
COPY . .

# Plain build of the exchange crate, with no cache mounts, so the same file
# builds under plain Docker, BuildKit and a registry's own build service.
RUN cargo build --release -p exchange \
    && cp target/release/exchange /app/exchange

# ──────────────────────────── runtime ────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates for outbound TLS/WSS to Gemini and the signal venues.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user, with an owned /data (the host sets SIGNALS_LANE_ROOT=/data/signals)
# and an owned working directory so the registry.json write at startup succeeds.
RUN useradd --create-home --uid 10001 app \
    && mkdir -p /app /data \
    && chown app:app /app /data

WORKDIR /app

# Data the binary opens at run time, at the same relative paths it expects.
COPY --chown=app:app copy ./copy
COPY --chown=app:app definitions ./definitions
# The start command is `./bin/exchange`, matching railway.toml / railpack.json.
COPY --from=build --chown=app:app /app/exchange ./bin/exchange

USER app

EXPOSE 8080

CMD ["./bin/exchange"]
