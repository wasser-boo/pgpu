# pgpu Router-Image: multi-stage — Router (debian) + gpu-agent (musl).
# Der Agent wird aus dem praxis-gpu-agent-Repo gebaut (named context `agent`,
# siehe deploy/build.sh) und liegt unter /gpu-agent für die Fork-Images
# (COPY --from=<router-image> /gpu-agent /opt/praxis/bin/gpu-agent).
# Askama-Templates werden einkompiliert; static/ liegt im Image.

FROM rust:1-bookworm AS agent-builder
# TARGETPLATFORM (nur mit BuildKit/buildx gesetzt): Agent als musl-Binary pro
# Plattform — amd64 läuft auf den Vast-Boxen, arm64 fürs NAS (Multi-Arch).
# Ohne buildx (plain docker build) bleibt x86_64-musl wie bisher.
ARG TARGETPLATFORM
RUN T=$([ "${TARGETPLATFORM:-linux/amd64}" = "linux/arm64" ] && echo aarch64-unknown-linux-musl || echo x86_64-unknown-linux-musl) && \
    rustup target add "$T" && \
    apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/* && \
    echo "$T" > /rusttarget
ARG TARGETARCH=""
WORKDIR /agent
COPY --from=agent Cargo.toml ./
COPY --from=agent src ./src
# Cache-Mounts pro Arch (Multi-Arch-Parallel-Build darf target/Registry nicht
# ungeschützt teilen — Race beim Entpacken, s. praxis-Dockerfile 21.09.):
RUN --mount=type=cache,target=/agent-tgt \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    T=$(cat /rusttarget) && \
    CARGO_TARGET_DIR=/agent-tgt/${TARGETARCH:-host} cargo build --release --target "$T" && \
    cp "/agent-tgt/${TARGETARCH:-host}/$T/release/praxis-gpu-agent" /gpu-agent

FROM rust:1-bookworm AS router-builder
ARG TARGETARCH=""
WORKDIR /build
# Workspace-Metadaten zuerst (Layer-Cache für Deps), dann echte Quellen.
COPY Cargo.toml Cargo.lock* ./
COPY crates ./crates
RUN --mount=type=cache,target=/build-tgt \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    CARGO_TARGET_DIR=/build-tgt/${TARGETARCH:-host} cargo build --release && \
    cp "/build-tgt/${TARGETARCH:-host}/release/praxis-router" /praxis-router

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    useradd --system --uid 10001 --home /data pgpu
WORKDIR /opt/praxis
COPY --from=router-builder /praxis-router /opt/praxis/bin/praxis-router
COPY --from=agent-builder /gpu-agent /gpu-agent
COPY crates/router/static /opt/praxis/static
RUN mkdir -p /data && chown -R pgpu /data
USER pgpu
VOLUME /data
ENTRYPOINT ["/opt/praxis/bin/praxis-router"]
CMD ["--config", "/data/config.toml"]