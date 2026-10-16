"""Scenario of fixtures/queryapp: SQL constructs of reporting queries (window functions...) and the Python around
them. dynapp's tables (`reset_tables`); every route rolls back."""

REFERENCE_APP = "fixtures.queryapp.main:app"

# string.Formatter().parse() inputs: fields, conversions, nested specs, escapes and every error of CPython's parser
FMT_CASES = ['', 'abc', 'a{0}b', '{}', '{name}', '{a.b[c]}', '{x!r}', '{x!r:>5}', '{x:{w}}', '{x:>{w}.{p}f} tail', '{{', '}}', 'a{{b}}c', '{', '}', 'a}b', '{x', '{x!', '{x!rr}', '{x!r', '{x:', '{x:{', '{a{b}', '{a[}b]}', '{[}', '{0!s:}', 'é{ü}', '{:}{:x}']

STEPS: list = [
    # ---- window functions (fixtures/queryapp/sqlw.py)
    *[("GET", f"/sqlw/over/rank?how={h}", None) for h in ("row_number", "rank", "dense_rank")],
    ("GET", "/sqlw/over/empty", None),
    ("GET", "/sqlw/over/lists", None),
    ("GET", "/sqlw/over/sub", None),
    ("GET", "/sqlw/over/filter", None),
    # ---- CTEs, LATERAL, correlate()
    ("GET", "/sqlw/cte/named", None),
    ("GET", "/sqlw/cte/params", None),
    ("GET", "/sqlw/cte/params?low=20&high=2002", None),
    ("GET", "/sqlw/cte/chain", None),
    ("GET", "/sqlw/cte/scalar", None),
    *[("GET", f"/sqlw/lateral?how={h}", None) for h in ("literal", "true", "inner")],
    ("GET", "/sqlw/lateral/auto", None),
    *[("GET", f"/sqlw/correlate?how={h}", None) for h in ("owner", "none")],
    # ---- case(*whens)
    *[("GET", f"/sqlw/case/star?how={h}", None) for h in ("list", "gen", "mixed")],
    *[("GET", f"/sqlw/case/empty?how={h}", None) for h in ("empty", "list")],
    # ---- icontains / istartswith / iendswith, cast(..., Numeric)
    *[("GET", f"/sqlw/icontains?q={q}&how={h}", None) for h in ("icontains", "istartswith", "iendswith")
      for q in ("P", "1", "%", "_", "x")],
    ("GET", "/sqlw/numeric", None),
    *[("GET", f"/sqlw/union?how={h}", None) for h in ("all", "method", "exec")],
    *[("POST", "/sqlw/any", {"names": n}) for n in (["p1", "ann", "zz"], ["bob"], [])],
    *[("POST", "/sqlw/typed", {"tags": t}) for t in (["a"], ["b", "a"], [], ["z"])],
    ("GET", "/sqlw/fetchone/closed", None),
    ("GET", "/sqlw/aliased/sub", None),
    *[("GET", f"/sqlw/aliased/more?how={h}", None) for h in ("cte", "union", "scalar")],
    # ---- dependency factories, Formatter().parse(), is_integer(), builtins (fixtures/queryapp/lang.py)
    ("GET", "/lang/depf/ids?id=1&id=2", None),
    ("GET", "/lang/depf/ids?id=[3,%204]", None),
    ("GET", "/lang/depf/ids?id=%5B%225%22%5D", None),
    ("GET", "/lang/depf/ids", None),
    ("GET", "/lang/depf/ids?id=", None),
    ("GET", "/lang/depf/ids?id=x", None),
    ("GET", "/lang/depf/required?name=a&name=b", None),
    ("GET", "/lang/depf/required", None),
    ("GET", "/lang/depf/flag", None),
    ("GET", "/lang/depf/flag?value=no", None),
    *[("POST", "/lang/fmt/parse", {"s": f}) for f in FMT_CASES],
    ("POST", "/lang/fmt/parse", {"s": 5}),
    *[("POST", "/lang/fmt/fields", {"s": f}) for f in ("Dear {name}, {greeting}!", "{a}{b}{a}", "{x", "no field")],
    ("POST", "/lang/isint", {"values": [53300.0, 1.5, 7, -0.0, 1e15, "s", True]}),
    ("GET", "/lang/isint/inf", None),
    ("GET", "/lang/builtins", None),
    ("GET", "/lang/chain", None),
    ("GET", "/lang/chain/bad", None),
    ("GET", "/lang/float-int", None),
    # ---- typing.NamedTuple (fixtures/queryapp/ntuple.py)
    ("GET", "/nt/basics", None),
    ("GET", "/nt/errors", None),
    ("GET", "/nt/response", None),
]


def reset(db: str) -> None:
    from sqlalchemy import create_engine

    from fixtures.queryapp.models import QBase
    from tests.scenarios.dynapp import reset_tables

    reset_tables(db)
    engine = create_engine(db.replace("postgresql://", "postgresql+psycopg://", 1))
    QBase.metadata.create_all(engine)
    with engine.begin() as conn:
        conn.exec_driver_sql("TRUNCATE qry_links, qry_nodes")
    engine.dispose()
