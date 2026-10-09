# Getting started

This guide takes you from `pip install py2axum` to a binary serving a FastAPI application, using the example in
[`examples/bookshelf`](https://github.com/larrymotalavigne/py2axum/tree/main/examples/bookshelf): a reading-list
API with users, JWT authentication, books, reviews and a WebSocket. Then it shows how to do the same with your
own application.

1. [Installation](install.md): install py2axum and get the examples.
2. [First application](first-app.md): generate and build the crate, run the binary, compare it with FastAPI,
   then do the same with your own application.
3. [`py2axum check`](check.md): what translates, route by route, and what to do when something is refused.
4. [Hybrid mode](hybrid.md): the routes left to a Python process next to the binary.

Deploying the binary (Docker, Kubernetes) is covered in [Deployment](../advanced/deployment.md), its settings in
[Environment variables](../reference/environment.md).

## What you need

- **Python ≥ 3.12**, at least as recent as the one your application targets: the transpiler parses your code
  with its own `ast` module, which only knows its own syntax.
- **Rust** (stable, via [rustup](https://rustup.rs)) to compile the generated crate. The first release build
  takes a few minutes; later builds only recompile your code.
- **PostgreSQL**: the runtime's database layer targets PostgreSQL (through sqlx).
- An application built on **FastAPI, Pydantic v2 and SQLAlchemy 2.0** (async or sync sessions), within the
  [tested version ranges](../reference/versions.md).
