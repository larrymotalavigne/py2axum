# Libraries translated from source

Most third-party libraries reach the binary through the closed [library map](../reference/libraries.md): a native
equivalent written by hand. `--translate-libs` adds a second way for pure-Python libraries: py2axum reads the
library's source in the project's environment and compiles it like project code, only the functions the routes
reach.

```bash
py2axum check app --translate-libs auto                 # what would translate, library by library
python -m py2axum app -o out --translate-libs auto        # every eligible distribution
python -m py2axum app -o out --translate-libs humanize,python-slugify
```

`--translate-libs` is off (`none`) by default, and **experimental** in 1.0: it is outside the
[stability guarantees](../reference/versioning.md#what-does-not-count-as-a-breaking-change) (it may change in a minor
release, and the release notes say so), because only a minority of pure-Python libraries translate so far
(`py2axum check --translate-libs auto` tells you which of yours do). It is accepted by generation, `py2axum check`
and `py2axum watch`, with `--venv` and `--allow-license`.

## What is translated from source

For each module an import names, the first of these wins:

1. a module of the project (under `--root`);
2. the library map, when it covers the module (`dateutil.relativedelta` stays native while `dateutil.parser`
   comes from its source); the framework (FastAPI, Starlette, Pydantic, SQLAlchemy, their drivers) and the
   standard library always stay with the translator;
3. the source of a selected distribution;
4. otherwise the call is refused at translation time with its `file:line`, as without the option.

A distribution is translated from source only when all of these hold, and refused with its own code otherwise:

- it is selected: `auto` selects every distribution of the environment, a list names them (as published, `-` or
  `_` alike);
- it is pure Python: a distribution with a compiled extension is refused
  ([P2A0702](../reference/errors.md#p2a0702)). Translating the pure-Python fallback of a C accelerator would not
  translate what CPython runs;
- its licence is admitted ([P2A0701](../reference/errors.md#p2a0701), see below);
- it is installed at the version the project locks (its `uv.lock`, else an `==` pin of a `requirements*.txt`), and
  its files match the hashes of its `RECORD` ([P2A0703](../reference/errors.md#p2a0703)): the binary carries the
  code the project runs.

The library's own code then follows the same subset as project code: a construct outside it is refused at its
`file:line` inside the library, and `py2axum check` lists the library's functions it reached.

## Libraries the map covers in part

The library map translates some names of a few libraries natively (`jinja2.Environment`, `aiosmtplib.send`,
`pywebpush.webpush`, the exceptions of `requests`). The names it does not cover (`jinja2.Template`,
`aiosmtplib.SMTP`, `requests.utils.requote_uri`) are read from the library's source, under the same conditions
as any distribution; the names it covers stay native everywhere, the library's own code included: an exception the
source raises (`requests.exceptions.InvalidURL`) derives from the map's class (`requests.RequestException`), so
the project's `except` clauses catch it as in Python.

One binary never holds both a non-exception name of the map and the source of the same library: an `Environment`
the map creates is no instance of the source's class, and the source's code does not take it. When a route that
translates uses the map's names, the routes reaching the library's source are refused
([P2A0705](../reference/errors.md#p2a0705)); with `--python-side auto` they run in Python and the others stay
native.

## The environment

The site-packages read is `--venv`'s, else the one of a `.venv` at `--root` or above (up to the repository's
root), else the one of the environment py2axum runs in (a Docker build that installs the project next to
py2axum). Translate in the environment the application runs in: optional imports (`try: import x / except
ImportError`) are decided by what is installed there.

Tests of `sys.version_info` follow the project's Python, as for project code; `sys.platform` and `os.name` follow
the platform py2axum runs on (build the binary where it runs).

## The standard library

With `--translate-libs` (any value but `none`), a closed list of pure-Python modules of the standard library is
translated from the `Lib/` of the Python py2axum runs on, which must be the project's minor
([P2A0704](../reference/errors.md#p2a0704)): `colorsys`, `gettext` (`NullTranslations`; reading `.mo`
catalogs, and the plural forms' warning of a non-int, are refused where reached), `html.entities`, `keyword`
(each one conformance-tested). The library map keeps precedence (`html.unescape` stays native). A module with a C
accelerator is never translated from `Lib/`, because CPython runs the compiled code: `bisect` (whose `_bisect`
raises other messages than `Lib/bisect.py`, "lo cannot be negative" from 3.15 among them) and `heapq` stay
refused ([P2A0702](../reference/errors.md#p2a0702)).

## Import-time behaviour

A library module's statements run at startup, in their place, as CPython runs them at import (a loop filling a
table, `codepoint2name[cp] = name`). A module-level `try: import x / except ImportError:` takes the handler when `x`
is not installed in the environment, as at import. A package's `__getattr__` (PEP 562) is read in the two shapes
libraries use for lazy imports: a literal map of names to modules (`target, is_submodule = _import_map[name]`)
and `if name in __all__: return importlib.import_module("." + name, __name__)`.

## Licences

The translation of a library is a derived work compiled into your binary. The licences admitted by default are
MIT, BSD (0BSD, BSD-2-Clause, BSD-3-Clause, BSD-4-Clause), Apache-2.0, ISC, PSF-2.0, Zlib, HPND, MPL-2.0 and Unlicense.
Three of these are the project's choice, made per licence: **BSD-4-Clause** (its advertising clause obliges the
binary's documentation to acknowledge the library's author), **MPL-2.0** (file-level copyleft: changes to the
library's own files stay under the MPL) and, per distribution, **passlib 1.7** (BSD-3-Clause, with a few functions
derived from Beer-ware, UnixCrypt and jBCrypt sources, whose licences py2axum does not recognise: no other
distribution benefits from it). The LGPL and GPL remain refused.
 py2axum reads the
licence, in this order:

1. the distribution's `License-Expression` (PEP 639);
2. its older `License` field, when it holds an SPDX expression of known identifiers (`BSD-2-Clause`);
3. when that field is free text (`BSD`, `BSD-like`: nothing a program can trust), the **licence file** shipped in the
   distribution's `.dist-info`, compared word for word with the canonical texts (MIT, BSD-2/3/4-Clause, ISC, 0BSD,
   Zlib, HPND, Unlicense; Apache-2.0 by the hash of its terms). Only holder names, copyright lines and
   punctuation may differ. A file holding several licences must have every one recognised; a modified licence
   (a rider such as "no commercial use"), a licence py2axum does not know
   (GPL, LGPL, Creative Commons, a bespoke permission) leaves text that nothing explains, and the library is refused;
4. the licence classifiers (a `License :: Other/Proprietary License` or `DFSG approved` classifier names no licence:
   the file then decides). With no `License` field, the classifiers come first, then the file.

An `OR` expression is admitted when one alternative is, an `AND` expression when all terms are. Anything else, an
unknown licence included, is refused with the text that was found, unless you admit it explicitly after checking it
allows your use: `--allow-license <SPDX>` for a recognised expression, `--allow-license <distribution>` for a
distribution whose licence could not be recognised:

```bash
python -m py2axum app -o out --translate-libs auto --allow-license MPL-2.0
```

## Cost

Each line of library code reached adds about 0.5 KB to the generated `gen.rs`, and rustc's peak memory grows by
about 0.53 GiB per MB of `gen.rs`. `py2axum check` prints these estimates per library: a large library reached by a
single route can make the crate too big for a small build machine.

The folded trees of the library files are cached in `~/.cache/py2axum/libs/` (or `$XDG_CACHE_HOME/py2axum/libs/`),
one directory per distribution, version, `RECORD` digest, py2axum version and Python; the functions are compiled
again for each project.

## Not translated

- package data (`pkgutil.get_data`, `importlib.resources`, files next to `__file__`) is refused;
- a distribution with a compiled extension, even when the reached modules are pure Python;
- the framework and the standard library (see above).
