"""Scenario of fixtures/ormapp: hybrid attributes with a custom Comparator, hybrid value objects (plain class and
dataclasses), `@dataclass(eq=False)`, a composite value object with setters, `relationship(primaryjoin=...,
post_update=True)`. Tables of its own (fixtures/ormapp/models.py), created once and emptied; every route but
/post/commit rolls back."""

REFERENCE_APP = "fixtures.ormapp.main:app"

STEPS: list = [("GET", "/health", None)]
STEPS += [("GET", f"/cmp/filter_by?word={w}", None) for w in ("Trucks", "trucks", "PICKUP", "none", "TRUCKS!")]
STEPS += [("GET", f"/cmp/where?word={w}&op={op}", None) for w in ("TRUCKS", "zebra") for op in ("eq", "ne", "lt", "ge")]
STEPS += [("GET", f"/cmp/operate?word={w}&op={op}", None) for w in ("trucks", "PICKUP", "b")
          for op in ("eq", "ne", "lt", "le", "gt", "ge", "rev", "rlt")]
STEPS += [("GET", "/cmp/objects", None), ("GET", "/cmp/raise?op=lt", None), ("GET", "/cmp/raise?op=ge", None)]
STEPS += [("GET", f"/value/sql?word={w}&kind={k}&op={op}", None) for k in ("ci", "dc") for w in ("Trucks", "PICKUP", "m")
          for op in ("eq", "ne", "lt", "le", "gt", "ge", "rev", "rgt")]
STEPS += [("GET", f"/value/aliased?kind={k}", None) for k in ("ci", "dc")]
STEPS += [("GET", f"/value/order?kind={k}", None) for k in ("ci", "dc")]
STEPS += [("GET", f"/value/python?word={w}&other={o}", None)
          for w, o in (("SomeWord", "sOmEwOrD"), ("SomeWord", "XOmEwOrX"), ("abc", "ABD"), ("b", "A"))]
STEPS += [("GET", "/dataclass/eq", None)]
STEPS += [("GET", f"/point/where?op={op}", None) for op in ("doc", "ne", "ge", "le", "gt")]
STEPS += [("GET", "/point/python", None), ("GET", "/point/select", None), ("GET", "/point/nosetter", None)]
STEPS += [("GET", "/post/insert", None)]
STEPS += [("GET", f"/post/change?to={t}", None) for t in ("new", "none", "existing")]
STEPS += [("GET", f"/post/delete?what={w}", None) for w in ("both", "entry_first", "widget")]
STEPS += [("GET", "/post/many", None), ("GET", "/post/commit", None), ("GET", "/post/commit", None)]


def reset(db: str) -> None:
    """fixtures/ormapp's tables: created once (the servers keep their prepared plans), emptied."""
    from sqlalchemy import create_engine

    from fixtures.ormapp.models import Base

    engine = create_engine(db.replace("postgresql://", "postgresql+psycopg://", 1))
    Base.metadata.create_all(engine)
    with engine.begin() as conn:
        conn.exec_driver_sql(f"TRUNCATE {', '.join(t.name for t in Base.metadata.sorted_tables)} RESTART IDENTITY CASCADE")
    engine.dispose()
