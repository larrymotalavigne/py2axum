# syntax=docker/dockerfile:1
# The py2axum builder image (ghcr.io/larrymotalavigne/py2axum): Python, py2axum, a pinned Rust toolchain and the
# runtime's dependencies precompiled. It turns a FastAPI application into a binary:
#
#   docker run --rm -v "$PWD:/app" ghcr.io/larrymotalavigne/py2axum build app --python-side auto
#
# or serves as the first stage of a multi-stage build (examples/docker/Dockerfile). Built from the repository root:
#   docker build -f docker/Dockerfile --build-arg PY2AXUM=py2axum==0.6.0 -t py2axum .   # a published version
#   docker build -f docker/Dockerfile --build-arg PY2AXUM=/src -t py2axum .              # this checkout
# Debian bookworm (glibc 2.36): the binaries it produces run on bookworm, trixie, Ubuntu 24.04 and
# gcr.io/distroless/cc-debian12.

ARG PYTHON_VERSION=3.14.8
ARG RUST_VERSION=1.99.0

FROM rust:${RUST_VERSION}-slim-bookworm AS rust

FROM python:${PYTHON_VERSION}-slim-bookworm
# what pip installs: a requirement (py2axum==X.Y.Z) or /src, the build context
ARG PY2AXUM=py2axum
ARG RUST_VERSION

LABEL org.opencontainers.image.title="py2axum" \
      org.opencontainers.image.description="Compile a FastAPI + SQLAlchemy + Pydantic application to a Rust (axum) binary" \
      org.opencontainers.image.source="https://github.com/larrymotalavigne/py2axum" \
      org.opencontainers.image.documentation="https://larrymotalavigne.github.io/py2axum/advanced/docker/" \
      org.opencontainers.image.licenses="Apache-2.0"

# the C toolchain links the binary and builds the C parts of a few crates (ring, zlib)
RUN apt-get update \
 && apt-get install -y --no-install-recommends gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    CARGO_TARGET_DIR=/opt/py2axum/target \
    PATH=/usr/local/cargo/bin:$PATH \
    PIP_DISABLE_PIP_VERSION_CHECK=1
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY --from=rust /usr/local/cargo /usr/local/cargo

RUN --mount=type=bind,target=/src,rw pip install --no-cache-dir "$PY2AXUM" && py2axum --help >/dev/null

COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/py2axum-entrypoint
COPY --chmod=755 docker/py2axum-build /usr/local/bin/py2axum-build

# Compile a one-route application once: every crate of the runtime lands in CARGO_TARGET_DIR, an application's build
# compiles only its own crate. The application's own artefacts and the extracted sources are dropped (cargo
# re-extracts from the downloaded .crate files when needed, offline). Any user may build (`docker run --user`).
RUN --mount=type=bind,source=docker/warm,target=/warm \
    useradd --uid 1000 --create-home py2axum \
 && cd /warm && py2axum-build app --name warm --no-check --out /tmp/warm-bin \
 && rm -rf /tmp/warm-bin "$CARGO_TARGET_DIR"/release/warm "$CARGO_TARGET_DIR"/release/warm.d \
           "$CARGO_TARGET_DIR"/release/deps/warm-* "$CARGO_TARGET_DIR"/release/.fingerprint/warm-* \
           "$CARGO_HOME"/registry/src \
 && chown -R py2axum:py2axum "$CARGO_HOME" /opt/py2axum \
 && chmod -R a+rwX "$CARGO_HOME" /opt/py2axum \
 && mkdir /app && chown py2axum:py2axum /app

USER py2axum
WORKDIR /app
ENTRYPOINT ["py2axum-entrypoint"]
CMD ["build", "--help"]
