"""loguru's formats compiled at translation (dynrt/loguru.rs renders them): a port of loguru 0.7's colorizer
(`_colorizer.py`: its markup tags, nesting rules and ANSI codes) and of its time formatting (`_datetime.py`).

`compile_format(fmt)` returns the Rust expression of a `dynrt::loguru::Format`: the text without colours, and
one per level with them (TRACE ... CRITICAL, then the levels given by number). A format loguru would refuse, or
a field whose rendering the binary does not reproduce, raises ValueError with the reason.
"""
from __future__ import annotations

import re
from string import Formatter

RT = "dynrt"

DEFAULT_FORMAT = ("<green>{time:YYYY-MM-DD HH:mm:ss.SSS}</green> | "
                  "<level>{level: <8}</level> | "
                  "<cyan>{name}</cyan>:<cyan>{function}</cyan>:<cyan>{line}</cyan> - <level>{message}</level>")

# loguru's levels (_defaults.py): name, colour markup
LEVEL_COLORS = [("TRACE", "<cyan><bold>"), ("DEBUG", "<blue><bold>"), ("INFO", "<bold>"), ("SUCCESS", "<green><bold>"),
                ("WARNING", "<yellow><bold>"), ("ERROR", "<red><bold>"), ("CRITICAL", "<RED><bold>")]

_STYLE = {"b": 1, "d": 2, "n": 22, "h": 8, "i": 3, "l": 5, "s": 9, "u": 4, "v": 7, "bold": 1, "dim": 2, "normal": 22,
          "hide": 8, "italic": 3, "blink": 5, "strike": 9, "underline": 4, "reverse": 7}
_COLORS = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]
_SHORT = ["k", "r", "g", "y", "e", "m", "c", "w"]
_FORE = {**{n: 30 + i for i, n in enumerate(_COLORS)}, **{n: 30 + i for i, n in enumerate(_SHORT)},
         **{"light-" + n: 90 + i for i, n in enumerate(_COLORS)}, **{"l" + n: 90 + i for i, n in enumerate(_SHORT)}}
_BACK = {**{n.upper(): 40 + i for i, n in enumerate(_COLORS)}, **{n.upper(): 40 + i for i, n in enumerate(_SHORT)},
         **{"LIGHT-" + n.upper(): 100 + i for i, n in enumerate(_COLORS)},
         **{"L" + n.upper(): 100 + i for i, n in enumerate(_SHORT)}}
_TAG = re.compile(r"(\\*)(</?(?:[fb]g\s)?[^<>\s]*>)")
TEXT, ANSI, LEVEL, CLOSING = 1, 2, 3, 4


def _ansi(tag: str) -> str | None:
    for table in (_STYLE, _FORE, _BACK):
        if tag in table:
            return "\033[%dm" % table[tag]
    if tag.startswith(("fg ", "bg ")):
        st, color = tag[:2], tag[3:]
        code = "38" if st == "fg" else "48"
        if st == "fg" and color.lower() in _FORE:
            return "\033[%dm" % _FORE[color.lower()]
        if st == "bg" and color.upper() in _BACK:
            return "\033[%dm" % _BACK[color.upper()]
        if color.isdigit() and int(color) <= 255:
            return "\033[%s;5;%sm" % (code, color)
        if re.match(r"#(?:[a-fA-F0-9]{3}){1,2}$", color):
            h = color[1:]
            if len(h) == 3:
                h *= 2
            return "\033[%s;2;%s;%s;%sm" % (code, *(int(h[i:i + 2], 16) for i in (0, 2, 4)))
        if color.count(",") == 2:
            parts = color.split(",")
            if all(x.isdigit() and int(x) <= 255 for x in parts):
                return "\033[%s;2;%s;%s;%sm" % (code, *parts)
    return None


class _Parser:
    """loguru's AnsiParser"""

    def __init__(self):
        self.tokens: list = []
        self.tags: list = []
        self.color_tokens: list = []

    def feed(self, text: str, raw: bool = False) -> None:
        if raw:
            self.tokens.append((TEXT, text))
            return
        pos = 0
        for m in _TAG.finditer(text):
            escaping, markup = m.group(1), m.group(2)
            self.tokens.append((TEXT, text[pos:m.start()]))
            pos = m.end()
            backslashes = "\\" * (len(escaping) // 2)
            if len(escaping) % 2 == 1:
                self.tokens.append((TEXT, backslashes + markup))
                continue
            if escaping:
                self.tokens.append((TEXT, backslashes))
            closing = markup[1] == "/"
            tag = markup[2:-1] if closing else markup[1:-1]
            if closing:
                if self.tags and (tag == "" or tag == self.tags[-1]):
                    self.tags.pop()
                    self.color_tokens.pop()
                    self.tokens.append((CLOSING, "\033[0m"))
                    self.tokens.extend(self.color_tokens)
                    continue
                if tag in self.tags:
                    raise ValueError('Closing tag "%s" violates nesting rules' % markup)
                raise ValueError('Closing tag "%s" has no corresponding opening tag' % markup)
            if tag in ("lvl", "level"):
                token = (LEVEL, None)
            else:
                ansi = _ansi(tag)
                if ansi is None:
                    raise ValueError('Tag "%s" does not correspond to any known color directive, make sure you did '
                                     "not misspelled it (or prepend '\\' to escape it)" % markup)
                token = (ANSI, ansi)
            self.tags.append(tag)
            self.color_tokens.append(token)
            self.tokens.append(token)
        self.tokens.append((TEXT, text[pos:]))

    def done(self, strict: bool = True) -> list:
        if strict and self.tags:
            raise ValueError('Opening tag "<%s>" has no corresponding closing tag' % self.tags[0])
        return self.tokens


def _prepare(string: str, depth: int = 2, recursive: bool = False) -> list:
    """Colorizer._parse_without_formatting: the format's tokens, its fields kept as written"""
    if depth < 0:
        raise ValueError("Max string recursion exceeded")
    p = _Parser()
    for literal, field, spec, conv in Formatter().parse(string):
        if literal and literal[-1] in "{}":
            literal += literal[-1]
        p.feed(literal, raw=recursive)
        if field is not None:
            text = "{" + field + ("!" + conv if conv else "") + (":" + spec if spec else "") + "}"
            p.feed(text, raw=True)
            _prepare(spec, depth - 1, True)
    return p.done()


def ansify(markup: str) -> str:
    p = _Parser()
    p.feed(markup.strip())
    return "".join(v for t, v in p.done(strict=False) if t != LEVEL)


def _colorize(tokens: list, ansi_level: str) -> str:
    return "".join(ansi_level if t == LEVEL else v for t, v in tokens)


def _strip(tokens: list) -> str:
    return "".join(v for t, v in tokens if t == TEXT)


# ---------------------------------------------------------------- fields

_FIELDS = {"level": "Level", "level.name": "LevelName", "level.no": "LevelNo", "level.icon": "LevelIcon",
           "name": "Name", "function": "Function", "line": "Line", "message": "Message", "module": "Module",
           "file": "File", "file.name": "File", "file.path": "FilePath", "exception": "Exception"}
# a sample value of each, to check a format spec as `format()` would
_SAMPLE = {"LevelNo": 20, "Line": 1}

_TOKENS = r"H{1,2}|h{1,2}|m{1,2}|s{1,2}|S+|YYYY|YY|M{1,4}|D{1,4}|Z{1,2}|zz|A|X|x|E|Q|dddd|ddd|d"
_TIME = re.compile(r"(?:{0})|\[(?:{0}|!UTC|)\]".format(_TOKENS))
_LOGURU_T = {"YYYY": "Year", "YY": "Year2", "Q": "Quarter", "MMMM": "MonthName", "MMM": "MonthAbbr", "MM": "Month2",
             "M": "Month", "DDDD": "YDay3", "DDD": "YDay", "DD": "Day2", "D": "Day", "dddd": "WeekdayName",
             "ddd": "WeekdayAbbr", "d": "Weekday", "E": "WeekdayIso", "HH": "Hour2", "H": "Hour", "hh": "Hour12x2",
             "h": "Hour12", "mm": "Min2", "m": "Min", "ss": "Sec2", "s": "Sec", "A": "AmPm", "Z": "Tz(true)",
             "ZZ": "Tz(false)", "zz": "TzName", "X": "Timestamp", "x": "TimestampMicro",
             **{"S" * n: f"Frac({n})" for n in range(1, 7)}}
_STRFTIME = {"Y": "Year", "y": "Year2", "m": "Month2", "d": "Day2", "H": "Hour2", "I": "Hour12x2", "M": "Min2",
             "S": "Sec2", "f": "Frac(6)", "z": "Tz(false)", "Z": "TzName", "j": "YDay3", "a": "WeekdayAbbr",
             "A": "WeekdayName", "b": "MonthAbbr", "B": "MonthName", "p": "AmPm"}


def rs(s: str) -> str:
    out = []
    for ch in s:
        if ch in '"\\':
            out.append("\\" + ch)
        elif ch == "\n":
            out.append("\\n")
        elif ord(ch) < 0x20 or ord(ch) == 0x7f:
            out.append("\\u{%x}" % ord(ch))
        else:
            out.append(ch)
    return '"' + "".join(out) + '"'


def _time_parts(spec: str) -> tuple[list[str], bool]:
    """`{time:spec}`: loguru's `_compile_format` (0.7.3), its tokens or strftime's directives"""
    utc = spec.endswith("!UTC")
    if utc:
        spec = spec[:-4]
    if not spec:
        spec = "%Y-%m-%dT%H:%M:%S.%f%z"
    parts: list[str] = []

    def lit(t: str) -> None:
        if t:
            parts.append(f"{RT}::loguru::T::Lit({rs(t)})")
    if "%" in spec:
        i = 0
        while i < len(spec):
            j = spec.find("%", i)
            if j < 0:
                lit(spec[i:])
                break
            lit(spec[i:j])
            d = spec[j + 1:j + 2]
            if d == "%":
                lit("%")
            elif d in _STRFTIME:
                parts.append(f"{RT}::loguru::T::{_STRFTIME[d]}")
            else:
                raise ValueError(f"the time directive `%{d}` is not reproduced (supported: "
                                 + " ".join("%" + k for k in _STRFTIME) + " %%)")
            i = j + 2
        return parts, utc
    if "SSSSSSS" in spec:
        raise ValueError("loguru refuses more than six successive 'S' in a time format")
    pos = 0
    for m in _TIME.finditer(spec):
        lit(spec[pos:m.start()])
        pos = m.end()
        tok = m.group(0)
        if tok in _LOGURU_T:
            parts.append(f"{RT}::loguru::T::{_LOGURU_T[tok]}")
        else:
            lit(tok[1:-1])
    lit(spec[pos:])
    return parts, utc


def _segments(text: str) -> str:
    """A formatted text (fields as `{name:spec}`, colours already literal) as `&[Seg]`"""
    segs: list[str] = []
    for literal, field, spec, conv in Formatter().parse(text):
        if literal:
            segs.append(f"{RT}::loguru::Seg::Lit({rs(literal)})")
        if field is None:
            continue
        if "{" in (spec or ""):
            raise ValueError(f"a nested field in the format spec of `{{{field}}}` is not reproduced")
        if field == "time":
            if conv:
                raise ValueError(f"`{{time!{conv}}}` is not reproduced")
            parts, utc = _time_parts(spec or "")
            segs.append(f"{RT}::loguru::Seg::Time(&[{', '.join(parts)}], {'true' if utc else 'false'})")
            continue
        if field not in _FIELDS:
            raise ValueError(f"the record field `{{{field}}}` is not reproduced (supported: time, "
                             + ", ".join(_FIELDS) + ")")
        kind = _FIELDS[field]
        if conv not in (None, "r", "s"):
            raise ValueError(f"the conversion `!{conv}` is not reproduced")
        if conv and field in ("level", "file"):
            raise ValueError(f"`{{{field}!{conv}}}` is not reproduced (the repr of loguru's record object)")
        if spec:
            sample = _SAMPLE.get(kind, "x") if conv != "r" else "'x'"
            try:
                format(sample, spec)
            except (ValueError, TypeError) as e:
                raise ValueError(f"the format spec of `{{{field}:{spec}}}` fails in loguru's handler ({e})") from None
        segs.append(f"{RT}::loguru::Seg::Field({RT}::loguru::F::{kind}, {'true' if conv == 'r' else 'false'}, {rs(spec or '')})")
    return "&[" + ", ".join(segs) + "]"


def compile_format(fmt: str) -> str:
    """`logger.add(..., format=fmt)` (a text): loguru appends its terminator and `{exception}`"""
    try:
        tokens = _prepare(fmt + "\n{exception}")
    except ValueError as e:
        raise ValueError(f"loguru refuses this format: color markups could not be parsed correctly ({e})") from None
    plain = _segments(_strip(tokens))
    colored = [_segments(_colorize(tokens, ansify(color))) for _, color in LEVEL_COLORS] + [_segments(_colorize(tokens, ""))]
    return (f"{RT}::loguru::Format {{ plain: {plain}, colored: &[{', '.join(colored)}] }}")
