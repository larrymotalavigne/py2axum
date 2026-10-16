"""Scenario of fixtures/protoapp: protocol methods of plain classes (iteration, containers, truth, arithmetic),
NotImplemented, dead code, class-scope defaults, generic bases, module attributes rebound, lazy module attributes.
No database. Compare the logs too (--logs): the iterators log between the loop's bodies."""

STEPS: list = [("GET", "/health", None), ("GET", "/p/misc/5", None)]
STEPS += [("GET", f"/p/loop/{n}" + q, None) for n in (0, 1, 4) for q in ("", "?stop=2", "?fail_at=2", "?fail_at=9")]
STEPS += [("GET", f"/p/naturals/{n}", None) for n in (0, 3)]
STEPS += [("GET", f"/p/consume/{n}", None) for n in (0, 1, 4)]
STEPS += [("GET", "/p/bag" + q, None) for q in ("", "?probe=A", "?probe=c", "?probe=")]
STEPS += [("GET", f"/p/bag/{i}", None) for i in (0, 1, -1, 2, -3)]
STEPS.append(("GET", "/p/truth", None))
STEPS += [("GET", f"/p/truth/bad/{w}", None) for w in ("bool", "neg", "str", "iter", "noiter", "nolen", "noitem", "noset")]
STEPS += [("GET", "/p/vec" + q, None) for q in ("", "?k=0", "?k=-3")]
STEPS += [("GET", f"/p/vec/bad/{w}", None) for w in ("mulvec", "addstr", "raddstr", "nodiv", "noinv")]
STEPS.append(("GET", "/p/stop", None))
STEPS += [("GET", f"/p/misc/{v}", None) for v in (0, 1, 300, 301)]
# the module-level `conf.api_key = ...` then a route rebinding it: read back by the module's own function
STEPS += [("POST", "/p/key/one", None), ("POST", "/p/key/two", None), ("GET", "/p/misc/2", None)]

STEPS += [("GET", f"/p/flags/{s}", None) for s in ("abc", "%C3%A9t%C3%A9", "x%20y")]


def reset(db: str) -> None:
    pass
