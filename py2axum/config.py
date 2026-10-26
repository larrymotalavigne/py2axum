"""`[tool.py2axum]` in pyproject.toml: the options of an application, so that a plain `py2axum check` or
`py2axum -o out` runs with them.

The closest pyproject.toml with a `[tool.py2axum]` table, from the current directory up, is read. Its keys are the
long options of the command (`python-side` or `python_side`, `app`, `root`...), plus `package` (the positional
argument) and `out`. A flag takes `true`; a repeatable option a string or a list; paths are relative to the
pyproject.toml. The options are put before the command line's: a value given there wins, a repeatable one adds to
the file's. A key another command takes (`fail-under` while generating) is skipped; an unknown key is an error.
"""
from __future__ import annotations

import argparse
import sys
import tomllib
from pathlib import Path

TABLE = "tool.py2axum"
# options of every command, read from the file only by the commands that have them
KNOWN = {"package", "out", "root", "path", "app", "python-side", "name", "no-stream", "allow-untested-versions",
         "translate-libs", "venv", "allow-license", "python", "format", "fail-under", "run", "release", "no-build",
         "interval", "debounce", "stop-timeout"}
# command-line only: what the file must not set
NEVER = {"help", "explain", "json", "report"}


class ConfigError(ValueError):
    pass


def find(start: Path) -> tuple[Path, dict] | None:
    """The closest pyproject.toml with a `[tool.py2axum]` table, and the table."""
    for d in [start, *start.parents]:
        f = d / "pyproject.toml"
        if not f.is_file():
            continue
        try:
            data = tomllib.loads(f.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
            raise ConfigError(f"{f}: {e}") from None
        table = data.get("tool", {}).get("py2axum")
        if table is not None:
            if not isinstance(table, dict):
                raise ConfigError(f"{f}: [{TABLE}] must be a table")
            return f, table
    return None


def _action(ap: argparse.ArgumentParser, key: str) -> argparse.Action | None:
    for a in ap._actions:
        if key == "package" and a.dest == "package" and not a.option_strings:
            return a
        if f"--{key}" in a.option_strings:
            return a
    return None


def _is_path(a: argparse.Action) -> bool:
    return a.type is Path


def apply(argv: list[str], make_parser, start: Path | None = None) -> list[str]:
    """`argv` with the options of `[tool.py2axum]` in front (`make_parser` builds the command's parser)."""
    found = find(start or Path.cwd())
    if found is None:
        return argv
    file, table = found
    base = file.parent
    ap = make_parser()
    out: list[str] = []
    package = None
    for raw, value in table.items():
        key = raw.replace("_", "-")
        if key in NEVER:
            raise ConfigError(f"{file}: [{TABLE}] {raw} is a command-line option only")
        a = _action(ap, key)
        if a is None:
            if key in KNOWN:
                continue  # another command's option
            raise ConfigError(f"{file}: [{TABLE}] unknown key {raw!r} (known: {', '.join(sorted(KNOWN))})")
        flag = "-o" if key == "out" else f"--{key}"

        def text(v, raw=raw, a=a):
            if isinstance(v, bool) or not isinstance(v, (str, int, float)):
                raise ConfigError(f"{file}: [{TABLE}] {raw} must be a string or a number, not {type(v).__name__}")
            v = str(v)
            return str((base / v).resolve()) if _is_path(a) else v

        if key == "package":
            package = text(value)
        elif isinstance(a, argparse._StoreTrueAction):
            if not isinstance(value, bool):
                raise ConfigError(f"{file}: [{TABLE}] {raw} must be true or false")
            if value:
                out.append(flag)
        elif isinstance(a, argparse._AppendAction):
            for v in value if isinstance(value, list) else [value]:
                out += [flag, text(v)]
        else:
            out += [flag, text(value)]
    if package is not None and not _has_positional(argv, make_parser):
        out.insert(0, package)
    print(f"note: options from {file} [{TABLE}]: {' '.join(out) or '(none)'}", file=sys.stderr)
    return out + argv


def _has_positional(argv: list[str], make_parser) -> bool:
    """Whether the command line names the package itself."""
    probe = make_parser()
    a = _action(probe, "package")
    if a is None:
        return False
    a.nargs, a.default = "?", None
    for x in probe._actions:  # `-o` of watch may come from the file too
        x.required = False

    def error(message):
        raise SystemExit(2)

    probe.error = error
    try:
        ns, _ = probe.parse_known_args(argv)
    except SystemExit:
        return True  # an error in the command line: argparse reports it again, with the file's options
    return ns.package is not None
