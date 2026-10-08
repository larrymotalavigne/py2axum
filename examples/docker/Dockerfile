# syntax=docker/dockerfile:1
# Reference multi-stage build: FastAPI application -> py2axum -> one binary on a slim image.
# Builds examples/bookshelf from the repository root:
#   docker build -f examples/docker/Dockerfile -t bookshelf .
# For your application: set APP_DIR (the directory you run uvicorn from), PACKAGE (the application package)
# and NAME (the binary), adjust PY2AXUM_FLAGS, and install py2axum from PyPI (see the first stage).

ARG APP_DIR=examples/bookshelf
ARG PACKAGE=app
ARG NAME=bookshelf
# what stays in Python: here every route that does not translate (relayed to PY2AXUM_PYTHON_URL)
ARG PY2AXUM_FLAGS="--python-side auto"

# 1. transpile, on a Python at least as recent as the application's
FROM python:3.13-slim AS transpile
ARG APP_DIR
ARG PACKAGE
ARG NAME
ARG PY2AXUM_FLAGS
# this example installs py2axum from the checkout it lives in; in your project:
#   RUN pip install --no-cache-dir "py2axum==<version>"
COPY pyproject.toml README.md LICENSE NOTICE /py2axum/
COPY py2axum /py2axum/py2axum
RUN pip install --no-cache-dir /py2axum
WORKDIR /src
# the whole application, with its lock or requirements file: library behaviours follow the pinned versions
COPY ${APP_DIR}/ .
# fails with file:line if a route does not translate and is not left to Python
RUN py2axum check ${PACKAGE} --root . ${PY2AXUM_FLAGS} \
 && py2axum ${PACKAGE} --root . ${PY2AXUM_FLAGS} -o /crate --name ${NAME}

# 2. compile; BuildKit keeps the registry and the target directory between builds
FROM rust:1-bookworm AS build
ARG NAME
WORKDIR /crate
COPY --from=transpile /crate .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/crate/target \
    cargo build --release --locked && cp target/release/${NAME} /app-binary

# 3. ship the binary alone
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app-binary /usr/local/bin/app
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
USER nobody
# the binary handles SIGTERM like uvicorn (graceful shutdown, PY2AXUM_SHUTDOWN_TIMEOUT)
CMD ["app"]
