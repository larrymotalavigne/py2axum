# Versioning and deprecation

py2axum follows [Semantic Versioning](https://semver.org/) from **1.0.0**: a breaking change to the public
contract below needs a new major version, an addition a new minor version, a fix a patch release. Before 1.0,
the contract is frozen since 0.8: it changes only through the deprecation process described here, so that
1.0 starts from what you already run.

Every release lists its changes in [the release notes](../release-notes.md), with a **Deprecated** and a
**Removed** section when there are any.

## What is the public contract

**The command lines.** `py2axum <package>`, `py2axum check` and `py2axum watch`: the names of the options,
the values they accept, their defaults and the exit statuses (0 success, 1 refused or below `--fail-under`, 2
usage error). See [Command line](cli.md). The wording of the messages and of the text and Markdown output of
`check` is not part of it.

**The binary.** Its command-line options (those of uvicorn it understands, see
[Logging § Running the binary like uvicorn](../advanced/logging.md#running-the-binary-like-uvicorn)) and the
[environment variables](environment.md) it reads, with their defaults. The same holds for the variables of the
Python-side recorder, `py2axum.record`.

**The supported subset.** A construct translated natively in a 1.x release keeps being translated, with the
same observable behaviour (status, headers, body byte for byte, validation errors) as the Python it comes from.
In a minor release, a refused construct may become supported, and a [documented difference](../supported.md)
may become narrower (closer to Python). A construct that becomes refused is a breaking change, with one
exception: when a translation turns out to differ from Python, it is refused (or fixed) in a patch release —
refusing is what py2axum does rather than guess, and the release notes say so.

**The tested version ranges.** The [ranges](versions.md) of Python, FastAPI, Starlette, Pydantic and
SQLAlchemy widen in minor releases. Raising a minimum is announced one minor release ahead, in the release
notes.

**The JSON output of `check`.** `py2axum check --format json` follows the published schema,
[`check-schema.json`](check-schema.json). A minor release may add keys; removing, renaming or retyping one is
a breaking change. Read it as an extensible format: ignore keys you do not know. The construct labels
(`construction`) and the messages (`reason`, `error`, `suggestion`) are text for people, not identifiers:
match on `status` and `code`.

**The error codes.** Each `P2Axxxx` [code](errors.md) names one class of refusal, for good. A code is never
reused for another class; a class that disappears (because the construct became supported) keeps its number,
retired. A minor release may add codes, for example to split a class in two for a better explanation.

## What is not

- **The generated Rust project**: its layout, the names in it, its dependencies and `Cargo.lock`. Generate it
  again after each upgrade; do not edit it nor depend on its internals.
- **The runtime** (`dynrt`, copied into each crate): an implementation detail of the generated binary.
- **The Python modules of py2axum** (`import py2axum...`), except the recorder's `RecordMiddleware`, and the
  internal coverage report (`--report`, deprecated).
- **Performance figures**, memory use, and the format of the binary's own log lines that do not come from
  uvicorn's or from your application's logging (those follow Python, see [Logging](../advanced/logging.md)).

## How a deprecation works

1. The option, value or variable keeps working, and is marked deprecated in the documentation and in the
   **Deprecated** section of the release notes, with what to use instead.
2. Using it prints a warning, without changing the exit status: at generation and in `check` for an option
   (`warning[deprecated]: --backend has no effect since 0.4 and will be removed in 1.0: drop it`), at
   start-up, on standard error, for a variable the binary reads.
3. It is removed in the next major release at the earliest, and never less than one minor release after the
   deprecation was published. Before 1.0, removals are batched into 1.0.

A removed option is an error that names its replacement, rather than an option silently ignored.

## Scheduled for 1.0

| What | Deprecated since | Replacement |
|---|---|---|
| `--backend auto\|dyn` (generation and `check`) | 0.4 (no effect: one backend) | drop it; `--backend typed` is already an error |
| key `backend` of the JSON of `check` (always `"dyn"`) | 0.8 | none: ignore it |
| `--report FILE` (generation) | 0.8 | `py2axum check --format json` or `--format markdown`; `--report` stays an internal tool of the project, outside the stable interface |
