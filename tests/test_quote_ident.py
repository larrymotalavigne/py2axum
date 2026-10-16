"""Names known at translation are quoted by the translator (`dyn.quote_ident`), the others at run time
(`orm::quote_ident`): both must quote alike."""
import re
from pathlib import Path

from py2axum import dyn

ORM = Path(__file__).resolve().parent.parent / "py2axum" / "runtime" / "dynrt" / "orm.rs"


def test_same_reserved_words_as_the_runtime():
    src = ORM.read_text()
    block = re.search(r"const RESERVED: &\[&str\] = &\[(.*?)\];", src, re.S).group(1)
    assert set(re.findall(r'"([a-z_]+)"', block)) == dyn.RESERVED_SQL


def test_quoting():
    cases = {"tasks": "tasks", "end": '"end"', "user": '"user"', "Mixed": '"Mixed"', "_x$1": "_x$1",
             "1st": '"1st"', "café": '"café"', 'a"b': '"a""b"', "users": "users", "order_lines": "order_lines"}
    assert {k: dyn.quote_ident(k) for k in cases} == cases
