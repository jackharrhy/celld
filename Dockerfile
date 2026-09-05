# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.94.1
ARG CELLD_COMMIT=unknown

FROM rust:${RUST_VERSION}-bookworm AS build
ARG TARGETARCH
# `release` for shipped artifacts; a fast-loop caller passes `lab` to skip
# the fat-LTO relink and keep incremental state in the target cache.
ARG CELLD_PROFILE=release
WORKDIR /src
COPY Cargo.toml Cargo.lock clippy.toml ./
COPY crates ./crates
RUN --mount=type=cache,id=celld-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=celld-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=celld-target-${TARGETARCH},target=/src/target,sharing=locked \
    mkdir -p /out && \
    cargo build --profile "${CELLD_PROFILE}" --locked -p celld --bins && \
    install -m 755 "target/${CELLD_PROFILE}/celld" /out/celld && \
    install -m 755 "target/${CELLD_PROFILE}/celld-store-copy" /out/celld-store-copy

# The final image depends on this stage, so a break in the engine's tests or
# lints stops the build.
FROM build AS test
ARG TARGETARCH
COPY tools ./tools
RUN rustup component add clippy
# The ltx fault-injection oracle diffs databases with the sqlite3 CLI.
RUN apt-get update && \
    apt-get install -y --no-install-recommends sqlite3 && \
    rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,id=celld-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=celld-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=celld-target-${TARGETARCH},target=/src/target,sharing=locked \
    cargo test --profile "${CELLD_PROFILE}" --locked && \
    cargo clippy --profile "${CELLD_PROFILE}" --all-targets --locked -- -D warnings
RUN --mount=type=cache,id=celld-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=celld-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=celld-target-${TARGETARCH},target=/src/target,sharing=locked \
    CARGO_TARGET_DIR=/src/target cargo test --manifest-path tools/local-store-tests/Cargo.toml --locked && \
    CARGO_TARGET_DIR=/src/target cargo run --manifest-path tools/local-store-tests/Cargo.toml --locked \
      --bin conformance -- local /tmp/conformance.sqlite3 /tmp/conformance.json

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
ARG CELLD_COMMIT
ARG CELLD_VERSION=unknown
ARG CELLD_UPSTREAM_COMMIT=unknown
LABEL org.opencontainers.image.title="celld" \
      org.opencontainers.image.revision="${CELLD_COMMIT}" \
      org.opencontainers.image.version="${CELLD_VERSION}" \
      org.opencontainers.image.source="https://github.com/jackharrhy/celld" \
      dev.celld.upstream.revision="${CELLD_UPSTREAM_COMMIT}"
COPY --from=test /out/celld /usr/local/bin/celld
COPY --from=test /out/celld-store-copy /usr/local/bin/celld-store-copy
ENTRYPOINT ["/usr/local/bin/celld"]
