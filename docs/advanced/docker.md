# Docker image

`ghcr.io/larrymotalavigne/py2axum` is a builder image: Python, py2axum, a pinned Rust toolchain and the
runtime's Rust dependencies, already compiled. It turns your FastAPI application into a binary without
installing anything on your machine or in CI, and it is the first stage of a multi-stage build whose final
image holds the binary alone.

| Tag | |
|---|---|
| `X.Y.Z` | one py2axum release (`0.5.1`); pin this in production builds |
| `X.Y` | the latest patch release of a minor version (`0.5`) |
| `latest` | the latest release |

Each tag is a multi-arch image (`linux/amd64`, `linux/arm64`) built by GitHub Actions from the release
published on PyPI, with an SBOM and a provenance attestation
(`docker buildx imagetools inspect ghcr.io/larrymotalavigne/py2axum:latest --format '{{ json .SBOM }}'`).

What is inside:

- Python 3.14 (the transpiler must run on a Python at least as recent as your application's) and py2axum;
- Rust 1.99 (`rustc`, `cargo`), `gcc` for linking;
- the runtime's crates, compiled with the release profile py2axum generates (fat LTO, one codegen unit) into
  `CARGO_TARGET_DIR=/opt/py2axum/target`: a build compiles your application's crate and links, nothing else;
- Debian bookworm (glibc 2.36): the binaries run on bookworm, trixie, Ubuntu 22.04 and later, and
  `gcr.io/distroless/cc-debian12`;
- a non-root user, `py2axum` (uid 1000), working directory `/app`.

## Build a binary with `docker run`

Mount the directory you run uvicorn from on `/app`, then name the application package:

```bash
docker run --rm -v "$PWD:/app" ghcr.io/larrymotalavigne/py2axum:0.5 build app --python-side auto
```

```
native       GET /books                     app/routers/books.py:39
...
py2axum-build: dist/app (15M, 126 s)
```

`build` runs [`py2axum check`](../getting-started/check.md) (it stops with `file:line` when a route neither
translates nor is left to Python), translates the package, compiles it, and writes the binary to `dist/<name>`.

| Option | |
|---|---|
| `PACKAGE` | the package holding the FastAPI application (default `app`) |
| `--name NAME` | the binary's name (default: the package's directory name) |
| `--out DIR` | where the binary goes (default `dist`, relative to `/app`) |
| `--crate DIR` | keep the generated Rust crate there (default: a temporary directory) |
| `--no-check` | skip `py2axum check` |
| `--python-side`, `--no-stream`, `--root`, `--allow-untested-versions` | passed to py2axum ([command line](../reference/cli.md)) |

The binary is a Linux executable for the image's architecture: on an Apple silicon Mac it is `linux/arm64`;
add `--platform linux/amd64` to build for x86-64 servers (emulated, much slower).

Other commands: `docker run … check app --root .` is `py2axum check`, `docker run … py2axum --help` runs the
CLI, and any other command runs as is (`docker run -it … bash`).

!!! note "File ownership"
    The image runs as uid 1000. If your user has another uid on Linux and `/app` is not writable by uid 1000,
    run as yourself: `docker run --user "$(id -u):$(id -g)" …` (the toolchain and the target directory are
    writable by any user).

## Multi-stage build

The reference [`examples/docker/Dockerfile`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/docker/Dockerfile)
builds with the image and ships the binary on `gcr.io/distroless/cc-debian12:nonroot` (glibc, libgcc and CA
certificates; no shell, no package manager):

```dockerfile
FROM ghcr.io/larrymotalavigne/py2axum:0.5 AS build
COPY --chown=py2axum:py2axum . /app/
RUN py2axum-build app --name app --out /tmp/out --python-side auto

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /tmp/out/app /usr/local/bin/app
ENV HOST=0.0.0.0 PORT=8080
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/app"]
```

Copy your lock or requirements file with the application: py2axum reads the pinned library versions, and
library behaviours follow them. The reference file takes build arguments (`APP_DIR`, `PACKAGE`,
`PY2AXUM_FLAGS`, `PY2AXUM_IMAGE`), so it can often be used as is:

```bash
docker build -f examples/docker/Dockerfile --build-arg APP_DIR=. --build-arg PACKAGE=api \
  --build-arg PY2AXUM_IMAGE=ghcr.io/larrymotalavigne/py2axum:0.5.1 -t myapp .
```

[`compose.yaml`](https://github.com/larrymotalavigne/py2axum/blob/main/examples/docker/compose.yaml), next to
it, runs the bookshelf example in [hybrid mode](../getting-started/hybrid.md): PostgreSQL, the Python sidecar
for the routes left to Python, and the binary as the only entry point.

```bash
docker compose -f examples/docker/compose.yaml up --build     # from the repository root
curl -s localhost:8080/health
```

## Build time and size

The fat LTO link of the whole program dominates a build: the runtime's crates are precompiled, but LTO
optimises them again with your code. Measured on the bookshelf example:

| | |
|---|---|
| builder image | about 700 MB compressed (2.7 GB unpacked, 1 GB of it precompiled crates) |
| bookshelf build (`docker run … build`), 6 vCPU arm64 | about 2 min, all in the final link |
| bookshelf binary | 15 MB |
| final image on distroless (`examples/docker/Dockerfile`) | 68 MB unpacked, 15 MB compressed |

Pin `X.Y.Z` for reproducible builds: a newer py2axum may translate more routes, or the same routes
differently. A version's image may be rebuilt (base image fixes), always with the same py2axum, Python and
Rust versions.

## Building the image yourself

[`docker/Dockerfile`](https://github.com/larrymotalavigne/py2axum/blob/main/docker/Dockerfile) builds the image
from a published version or from a checkout:

```bash
docker build -f docker/Dockerfile --build-arg PY2AXUM=py2axum==0.5.1 -t py2axum .   # a release
docker build -f docker/Dockerfile --build-arg PY2AXUM=/src -t py2axum .              # this checkout
```

Build arguments `PYTHON_VERSION` and `RUST_VERSION` choose the toolchains.
