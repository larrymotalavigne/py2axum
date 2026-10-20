"""Scenario of fixtures/depapp: dependency forms of public applications (corpus/apps, family P2A0307): `yield`
under `with`/`async with`/`if`/nested `try`, inherited `__init__`, classmethods/staticmethods, factories with
`*roles` and chained factories, lambdas. No database. /events reads (and clears) what the dependencies recorded:
the order of entering and leaving, and the exception each context manager saw."""

# the `yield` routes are POSTs: FastAPI closes their dependencies once the response is sent, the pause after a write
# lets that land on both sides before /events reads it
SETTLE = 0.2
REFERENCE_APP = "fixtures.depapp.main:app"

EV = ("GET", "/events", None)
STEPS: list = [("GET", "/health", None)]
# yield under with / async with / if / try
STEPS += [("POST", "/g/awith", None), EV, ("POST", "/g/awith/fail", None), EV]
STEPS += [("POST", "/g/with", None), EV, ("POST", "/g/with/fail/http", None), EV, ("POST", "/g/with/fail/value", None), EV]
STEPS += [s for k in ("ok", "http", "value") for s in (("POST", f"/g/wtry/{k}", None), EV)]
STEPS += [s for k in ("ok", "fail") for s in (("POST", f"/g/swallow/{k}", None), EV)]
STEPS += [s for q in ("", "?mode=a", "?mode=b", "?mode=c") for s in (("POST", "/g/if" + q, None), EV)]
# class dependencies, inherited __init__, classmethods, staticmethods
STEPS += [("GET", "/k/pages", None), ("GET", "/k/pages?page=3&size=2", None), ("GET", "/k/pages?page=x", None),
          ("GET", "/k/pages2?size=4", None)]
STEPS += [s for p in ("/k/service", "/k/service?tag=q&page=2", "/k/service/api", "/k/service/api?size=bad",
                      "/k/cm", "/k/cm?tag=z", "/k/cm/api?page=5", "/k/cm/twice", "/k/cm/sync", "/k/cm/sync?size=7",
                      "/k/static", "/k/static?flag=true", "/k/static?flag=maybe", "/k/unfinished")
          for s in (("GET", p, None), EV)]
STEPS += [("GET", p, None, h) for p in ("/k/remote", "/k/remote?limit=9", "/k/remote?limit=x", "/k/remote/build",
                                       "/k/remote/build?limit=1") for h in ({}, {"x-token": "abc"})]
# factories with *roles, chained factories, aliases, lambdas
for role in (None, "admin", "operator", "guest", "nobody"):
    h = {} if role is None else {"x-role": role}
    STEPS += [s for p in ("/f/one", "/f/none", "/f/chained", "/f/chained/star", "/f/alias", "/f/two")
              for s in (("GET", p, None, h), EV)]
# dependencies and an Annotated alias bound in the application factory
for tok in (None, "Bearer t1", "Bearer t2", "Bearer zz", "Basic x"):
    h = {} if tok is None else {"authorization": tok}
    STEPS += [s for p in ("/fac/me", "/fac/twice", "/fac/admin") for s in (("GET", p, None, h), EV)]
STEPS += [("GET", "/f/lambda", None), ("GET", "/f/lambda/q", None), ("GET", "/f/lambda/q?q=v", None)]


def reset(db: str) -> None:
    pass
