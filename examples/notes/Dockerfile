# Build from the repository root (the example installs py2axum from this checkout):
#   docker build -f examples/notes/Dockerfile -t notes .
# In your own project, replace the first stage's install with:
#   RUN pip install --no-cache-dir git+https://github.com/larrymotalavigne/py2axum

# 1. transpile the FastAPI application into a Rust crate
FROM python:3.13-slim AS transpile
WORKDIR /py2axum
COPY pyproject.toml README.md LICENSE NOTICE ./
COPY py2axum ./py2axum
RUN pip install --no-cache-dir .
WORKDIR /src
COPY examples/notes/ .
RUN py2axum app --root . -o /crate --name notes

# 2. compile it
FROM rust:1-bookworm AS build
WORKDIR /crate
COPY --from=transpile /crate .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/crate/target \
    cargo build --release && cp target/release/notes /notes

# 3. ship the binary alone
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /notes /usr/local/bin/notes
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
USER nobody
CMD ["notes"]
