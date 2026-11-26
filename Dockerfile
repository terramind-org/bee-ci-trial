# syntax=docker/dockerfile:1.7
# Beehive server, Bee CLI and local daemon. Build from the repository root:
#   docker build -t bee-server .

ARG RUST_VERSION=1.99.0
# Parallel rustc jobs. Two keeps the build under 8 GiB of memory; raise it on a larger builder.
ARG CARGO_BUILD_JOBS=2

FROM rust:${RUST_VERSION}-bookworm AS builder
# The pinned toolchain is already in this image; naming it stops rustup fetching it again.
ARG RUST_VERSION
ARG CARGO_BUILD_JOBS
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION} \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p bee-server -p bee-cli -p bee-daemon \
    && mkdir -p /out \
    && cp target/release/bee-server target/release/bee target/release/bee-daemon /out/ \
    && strip --strip-unneeded /out/*

FROM debian:bookworm-slim AS runtime
ARG BUILD_SHA=unknown
LABEL org.opencontainers.image.title="bee-server" \
      org.opencontainers.image.description="Beehive server: source control and forge for AI agents" \
      org.opencontainers.image.source="https://github.com/terramind-org/bee" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.revision="${BUILD_SHA}"
# git: mirrors, exports and importers run the git binary. curl: the health check.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --home-dir /data --shell /usr/sbin/nologin bee \
    && mkdir -p /data /etc/bee \
    && chown bee:bee /data
COPY --from=builder /out/bee-server /out/bee /out/bee-daemon /usr/local/bin/
USER bee
WORKDIR /data
VOLUME /data
EXPOSE 7420 2222
HEALTHCHECK --interval=10s --timeout=3s --start-period=20s --retries=5 \
    CMD curl -fsS http://127.0.0.1:7420/healthz || exit 1
ENTRYPOINT ["bee-server"]
CMD ["--config", "/etc/bee/server.toml"]
