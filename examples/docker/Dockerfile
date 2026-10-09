# syntax=docker/dockerfile:1
# Reference multi-stage build: FastAPI application -> py2axum builder image -> one binary on a distroless image.
# Builds examples/bookshelf from the repository root:
#   docker build -f examples/docker/Dockerfile -t bookshelf .
# For your application: set APP_DIR (the directory you run uvicorn from, with its lock or requirements file:
# library behaviours follow the pinned versions), PACKAGE (the application package) and PY2AXUM_FLAGS, and pin
# the builder to the py2axum version you tested (ghcr.io/larrymotalavigne/py2axum:X.Y.Z, or X.Y).

ARG PY2AXUM_IMAGE=ghcr.io/larrymotalavigne/py2axum:latest
ARG APP_DIR=examples/bookshelf
ARG PACKAGE=app
# what stays in Python: here every route that does not translate (relayed to PY2AXUM_PYTHON_URL)
ARG PY2AXUM_FLAGS="--python-side auto"

# 1. translate and compile: Python, py2axum, Rust and the runtime's dependencies are already in the image, so only
#    the application's crate is compiled. Fails with file:line if a route neither translates nor is left to Python.
FROM ${PY2AXUM_IMAGE} AS build
ARG APP_DIR
ARG PACKAGE
ARG PY2AXUM_FLAGS
COPY --chown=py2axum:py2axum ${APP_DIR}/ /app/
RUN py2axum-build ${PACKAGE} --name app --out /tmp/out ${PY2AXUM_FLAGS}

# 2. ship the binary alone: glibc, libgcc and CA certificates, no shell, non-root
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tmp/out/app /usr/local/bin/app
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
# the binary handles SIGTERM like uvicorn (graceful shutdown, PY2AXUM_SHUTDOWN_TIMEOUT)
ENTRYPOINT ["/usr/local/bin/app"]
