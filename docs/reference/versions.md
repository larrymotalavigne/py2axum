# Supported versions

The runtime reproduces the behaviour of precise library versions (Pydantic's error messages and URLs,
Starlette's routing and middlewares, SQLAlchemy's session), so py2axum only accepts the ranges its
conformance suites run on. CI runs the test suite and the full conformance of the reference apps (`fixtures/dynapp`,
`langapp`, `integapp`, `libapp`; normal and forced-streaming passes) at both ends of every range: lowest versions on Python 3.12, highest on 3.14 and on
3.15, and highest with pydantic 2.13 on 3.13 (every supported pydantic minor is run).

| Library | Lowest tested | Highest tested | Accepted |
|---|---|---|---|
| fastapi | 0.137.0 | 0.156.8 | `>=0.137.0,<0.157` |
| starlette | 1.0.0 | 1.9.0 | `>=1.0.0,<1.10` |
| pydantic | 2.12.0 | 2.14.0 | `>=2.12.0,<2.15` |
| pydantic-core | 2.41.1 | 2.50.0 | `>=2.41.1,<2.51` |
| pydantic-settings | 2.11.0 | 2.16.0 | `>=2.11.0,<2.17` |
| sqlalchemy | 2.0.44 | 2.1.5 | `>=2.0.44,<2.2` |
| psycopg | 3.2.12 | 3.3.6 | `>=3.2.12,<3.4` |
| httpx | 0.28.1 | 0.28.1 | `>=0.28.1,<0.29` |
| aiohttp | 3.13.0 | 3.14.4 | `>=3.13.0,<3.15` |
| mcp | 2.2.0 | 2.2.0 | `>=2.2.0,<2.3` |
| asyncpg | 0.31.0 | 0.32.0 | `>=0.31.0,<0.33` |
| icalendar | 7.0.0 | 7.0.0 | `>=7.0.0,<7.1` |
| orjson | 3.12.0 | 3.13.0 | `>=3.12.0,<3.14` |
| unidecode | 1.4.0 | 1.4.0 | `>=1.4.0,<1.5` |
| ecs-logging | 2.3.0 | 2.3.0 | `>=2.3.0,<2.4` |
| pyyaml | 6.0.2 | 6.0.3 | `>=6.0.2,<6.1` |
| croniter | 6.2.4 | 6.2.4 | `>=6.2.4,<6.3` |
| itsdangerous | 2.2.0 | 2.2.0 | `>=2.2.0,<2.3` |
| dnspython | 2.8.0 | 2.8.0 | `>=2.8.0,<2.9` |
| pytz | 2026.5 | 2026.5 | `>=2026.5,<2026.6` |
| python-dateutil | 2.9.0 | 2.9.0.post0 | `>=2.9.0,<2.10` |
| cachetools | 7.2.2 | 7.2.2 | `>=7.2.2,<7.3` |
| stripe | 14.0.0 | 14.4.1 | `>=14.0.0,<14.5` |
| openpyxl | 3.1.5 | 3.1.5 | `>=3.1.5,<3.2` |
| et-xmlfile | 2.0.0 | 2.0.0 | `>=2.0.0,<2.1` |
| pgpy | 0.6.0 | 0.6.0 | `>=0.6.0,<0.7` |
| pyasn1 | 0.6.4 | 0.6.4 | `>=0.6.4,<0.7` |
| cryptography | 50.0.2 | 50.0.2 | `>=50.0.2,<51` |
| reportlab | 4.4.5 | 4.4.5 | `>=4.4.5,<4.4.6` |
| qrcode | 8.2 | 8.2 | `>=8.2,<8.3` |
| pillow | 12.3.0 | 12.3.0 | `>=12.3.0,<12.4` |
| loguru | 0.7.2 | 0.7.3 | `>=0.7.2,<0.8` |
| bcrypt | 4.3.0 | 5.0.0 | `>=4.3.0,<5.1` |
| passlib | 1.7.4 | 1.7.4 | `>=1.7.4,<1.8` |
| python-dotenv | 1.2.0 | 1.2.4 | `>=1.2.0,<1.3` |
| Python | 3.12 | 3.15 | `>=3.12,<3.16` |

`mcp` is covered by the conformance of a real MCP server (tested internally), rather than by the matrix;
`asyncpg` the same way, by the conformance of a real application served with that driver (0.31.0 and 0.32.0),
and `icalendar` by a real application's calendar export. `pgpy`, `pyasn1` and `cryptography` are checked only when the project imports pgpy, and covered by
`fixtures/pgpyapp` (the inputs measured on pgpy 0.6.0 with pyasn1 0.6.4 and cryptography 50.0.2: pyasn1 decodes the
curve OIDs, cryptography's deprecated ciphers are touched by the parsing of a protected secret key). `qrcode` and `pillow` are checked only when the project imports qrcode: the PNG is the one of Pillow's PyPI wheels, whose zlib differs by platform: zlib 1.3.1 in the Linux (manylinux) wheels, zlib-ng (2.3.3 in 12.3.0's) in the macOS ones; the binary compresses as the wheels of the platform it is built for. A Pillow built against another zlib compresses differently. Patch releases inside a range are accepted without being tested one by one. Behaviours that change
inside a range follow the project's version: CPython's messages and behaviours (3.14 names the role of an
unhashable dict key or set element and the expected input of a `math` domain error; the full list, from 3.12 to
3.15, is in [Python versions](../python-versions.md)), Pydantic's (see below), Starlette's `CORSMiddleware`. The project's version is the one of its `uv.lock`, else its `==` pin in `requirements*.txt`
or `pyproject.toml`, else the one installed next to py2axum (when the project's specifier allows it; otherwise
the highest tested version the specifier allows).

## Out of scope for 1.0

These are other frameworks, not older versions of the ones py2axum reproduces, and a project that locks one of them is
refused ([P2A0601](errors.md#p2a0601)) whatever the options:

- **Starlette 0.x** (below 1.0): the hooks, routing, form parsing and response classes changed at 1.0. FastAPI versions
  that require it are out with it.
- **Pydantic 1**: another validation engine and other messages.
- **SQLAlchemy 1.4**: the 1.x `Session` and `Query` semantics; 2.0 is the floor.
- **Python below 3.12**.

Inside the supported majors, the ranges above are widened only with proof: a version is added when the conformance
suites run on it and its differences from the neighbouring versions are reproduced (or refused), never by relaxing the
check. `--allow-untested-versions` translates a project outside the ranges at your own risk; `py2axum check` then lists the
constructs of the project that are known to differ between versions.

Pydantic behaviours that follow the project's minor version:

| Behaviour | 2.12 | 2.13 | 2.14 |
|---|---|---|---|
| Documentation URL of each error (`errors()`, `str(exc)`) | `…/2.12/v/…` | `…/2.13/v/…` | `…/2.14/v/…` |
| Invalid UUID message (uuid crate 1.23.0 → 1.23.4 in pydantic-core 2.50) | lists the expected characters, 1-based position | 1-based position, `invalid length: expected length 32 for simple format, found N` | 0-based position, `invalid length: found N`; a 36-character string, or a `{braced}` one of 38, is read as hyphenated (`invalid group count`); empty or longer than 45: `invalid length` |
| `EmailStr` with a CR or LF | accepted (stripped, or as the space of the `Name <addr>` form) | same | `value is not a valid email address: Carriage return and line feed characters are not allowed` (after `str_strip_whitespace`) |
| `Decimal` `max_digits`/`decimal_places` beyond 28 significant digits | the normalized value is rounded to 28 digits (`Decimal.normalize()`): `9.99…9` (29 digits) counts as `1E+1` | same | no rounding: trailing zeros stripped only |
| An Enum's `_missing_` that raises | any exception is the `enum` error | same | a `ValueError` is the `enum` error, any other exception propagates (500) |

Other pydantic 2.14 changes concern constructs py2axum refuses (`multiple_of`, `ser_json_timedelta` /
`ser_json_temporal`, `@field_serializer`, `deque`/`Counter`/`OrderedDict`/`NamedTuple` fields, a
`default_factory` taking the data, length constraints on `Iterable`) or JSON-mode validation only
(`model_validate_json`): a `Decimal` from a 3-item array (`Decimal((sign, digits, exponent))`) and an Enum
with a `None` member (2.13 returns it for any unknown JSON value) are not reproduced.

WebSocket routes reproduce Starlette 1.7
(`WebSocketDisconnected`): a project declaring them needs `starlette>=1.7.0`. Below the lowest versions, the
differences found were FastAPI's security schemes (401 vs 403, credential stripping), empty `Form` strings and
included-router objects (< 0.137), and Jinja2 autoescape of `.txt` templates (Starlette < 1.0).

The analysed project's versions are read from the first of these found at `--root` or above (up to the
repository's root): `uv.lock` (exact versions), else `requirements*.txt` and the `pyproject.toml`
dependencies. A locked or pinned version outside its range, or a specifier that excludes the whole range
(`sqlalchemy<2`), is refused with `file:line`, by generation, `check` and `--report` alike. A specifier that
overlaps the range (`fastapi>=0.100`) is accepted. `requires-python` must overlap 3.12–3.15, and its lower bound
must not be newer than the Python running py2axum (the parser only knows its own syntax). Libraries the project
does not mention are not checked. The Python the project runs on (whose messages the binary reproduces) is
`--python X.Y`, else the project's `.python-version`, else the Python running py2axum when `requires-python`
allows it, else the lowest one it allows: see [Python versions](../python-versions.md). `--allow-untested-versions` translates anyway, at your own risk.
