# Versioning and deprecation

py2axum follows [Semantic Versioning](https://semver.org/) from **1.0.0**: a breaking change to the public
contract below needs a new major version, an addition a new minor version, a fix a patch release. Before 1.0,
the contract is frozen since 0.8: it changes only through the deprecation process described here, so that
1.0 starts from what you already run.

Every release lists its changes in [the release notes](../release-notes.md), with a **Deprecated** and a
**Removed** section when there are any.

## What 1.0 guarantees

In short, from 1.0.0 and for every 1.x release:

- the three command lines, their options, values, defaults and exit statuses keep working as documented;
- the binary's options and environment variables keep their names, meanings and defaults;
- what translates natively keeps translating, with the same observable behaviour; refusals are explained by
  `P2Axxxx` codes that are never reused;
- `py2axum check --format json` keeps every key of its published schema, with its type;
- the tested ranges of Python, FastAPI, Starlette, Pydantic and SQLAlchemy only widen, and a raised minimum is
  announced one minor release ahead;
- anything marked **experimental** below is outside these guarantees until it is promoted.

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

**The generated binary's observable behaviour.** The responses it gives (status, headers, body byte for byte,
validation errors, the order of JSON keys), the exit status and the signals handling of the process, and the
`migrate` subcommands (`migrate [upgrade [head|<revision>]]`, `migrate current`, `migrate --sql`). This is the
contract that [conformance](../advanced/conformance.md) tests: the *files* of the Rust project are not (see
below), what the binary does is.

**Stability of each element.** Every option, subcommand and variable listed in [Command line](cli.md) and
[Environment variables](environment.md) is **stable**, except what this table says:

| Element | Status | Notes |
|---|---|---|
| `--translate-libs`, `--venv`, `--allow-license` (generation, `check`, `watch`) | experimental | off by default; they translate pure-Python libraries from their source, and how many libraries qualify is still small. May change in a minor release (release notes say so), and are promoted to stable once the feature is on by default or its coverage is measured as settled. The codes `P2A0701`–`P2A0705` of this feature follow the code rules above whatever the status |
| `PY2AXUM_SQL_DEBUG` | experimental | a debugging aid: the format of what it prints may change |
| `--report FILE` (generation) | internal, hidden | no longer in `--help` or the documentation; prints a deprecation warning; outside the contract, may disappear in any release |
| `--backend`, key `backend` of the JSON of `check` | removed in 1.0 | see "Removed in 1.0" at the end |
| `--json` (`check`) | stable alias | same as `--format json`; kept for good, no deprecation planned |
| `-o` / `--out` | stable alias | the two spellings are the same option |
| `PY2AXUM_TARGET_PYTHON` | stable alias | the variable form of `--python` for the translator |
| `PY2AXUM_LOG_LEVEL` | stable, legacy | kept for deployments that set it; `UVICORN_LOG_LEVEL` / `--log-level` is the way to write new ones |
| the recorder's variables (`PY2AXUM_RECORD*`) | stable | Python side only, see [Environment variables](environment.md) |

The wording of `--help`, of error and warning messages and of the text and Markdown reports is not part of the
contract (a test pins the help text so that a change is deliberate, not so that it never changes).

## What is not

- **The generated Rust project**: its layout, the names in it, its dependencies, its build profile and
  `Cargo.lock`. Generate it again after each upgrade; do not edit it nor depend on its internals. (0.9 turned
  the runtime, a module of the crate until then, into a library crate next to it, and the release profile from
  fat to thin LTO.)
- **The runtime** (`dynrt`, the library crate copied next to each generated crate): an implementation detail of
  the generated binary, not a crate to depend on.
- **The Python modules of py2axum** (`import py2axum...`), except the recorder's `RecordMiddleware`, and the
  internal coverage report (`--report`, deprecated).
- **`--path DIR`** (experimental, repeatable; generation and `check`): one more import root, like an extra `sys.path`
  entry, for the sibling packages of a monorepo that the application installs from its own checkout. It may change or
  go before 1.0.
- **Performance figures**, memory use, and the format of the binary's own log lines that do not come from
  uvicorn's or from your application's logging (those follow Python, see [Logging](../advanced/logging.md)).

## What does not count as a breaking change

These can happen in any minor or patch release:

- a construct that was refused becomes native; a [documented difference](../supported.md) narrows;
- a construct that turns out to differ from Python is refused or fixed (patch release, announced);
- new options, subcommands, environment variables, error codes, keys in the JSON of `check`, routes `check`
  reports as native instead of python-side, and wider tested ranges;
- a different Rust project from the same Python (layout, dependencies, build profile, the runtime's internals),
  a different compile time, binary size, throughput or memory use;
- new or reworded messages, warnings and help text; an `error[...]` line gaining a `= help:` or `= see:` line;
- experimental elements (above), and the internal tools (`--report`, `tools/`, the corpus and conformance
  harnesses);
- a new Python, FastAPI, Starlette, Pydantic or SQLAlchemy version entering the tested ranges, or a version
  *leaving* them after the announcement a minor release ahead (a project locked there is refused by `check`
  with `P2A0601` unless `--allow-untested-versions`).

## How a deprecation works

The period is **at least one minor release and at least three months** between the release that announces it
and the major release that removes it; since the contract is frozen, 1.0 batches the removals scheduled below.


1. The option, value or variable keeps working, and is marked deprecated in the documentation and in the
   **Deprecated** section of the release notes, with what to use instead.
2. Using it prints a warning, without changing the exit status: at generation and in `check` for an option
   (`warning[deprecated]: ...`), at
   start-up, on standard error, for a variable the binary reads.
3. It is removed in the next major release at the earliest, and never less than one minor release after the
   deprecation was published. Before 1.0, removals are batched into 1.0.

A removed option is an error that names its replacement, rather than an option silently ignored.

## Removed in 1.0

Decided on 2026-10-19 (the deprecations were announced in 0.4 and 0.8, see the [release notes](../release-notes.md)).
Each one is an error that says what to do, never a silent no-op.

| What | Deprecated since | What to do |
|---|---|---|
| `--backend auto\|dyn\|typed` (generation and `check`) | 0.4 (no effect: one backend) | drop it: `error: --backend was removed in 1.0 ...`, exit status 2 |
| key `backend` of the JSON of `check` (always `"dyn"`) | 0.8 | none: it is no longer emitted, and no longer in [`check-schema.json`](check-schema.json) |
| `--report FILE` (generation) | 0.8 | `py2axum check --format json` or `--format markdown`; `--report` stays an internal tool, hidden from `--help` and outside the stable interface |
