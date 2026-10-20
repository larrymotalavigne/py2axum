"""Scenario of fixtures/subprocapp: asyncio.create_subprocess_exec (pipes, return codes, signals, timeouts, spawn
errors). No database."""

STEPS: list = [("GET", "/health", None)]
STEPS += [("GET", "/sp/printf" + q, None) for q in ("", "?arg=%C3%A9t%C3%A9", "?fmt=%25s%5Cn%25s&arg=a", "?fmt=%25d&arg=x",
                                                   "?fmt=&arg=")]
STEPS += [("GET", f"/sp/star/{a}/{b}", None) for a, b in (("a", "b"), ("x%20y", "-"))]
STEPS += [("POST", "/sp/cat" + q, body) for q in ("", "?keyword=true") for body in (b"", b"abc", "é\u0000z".encode() * 3, b"x" * 300000, {"j": 1})]
STEPS += [("GET", f"/sp/big/{n}", None) for n in (0, 1, 65536, 1000000)]
STEPS += [("GET", "/sp/sh" + q, None) for q in ("", "?code=3", "?code=255", "?merge=true", "?devnull=true",
                                               "?merge=true&code=1")]
STEPS += [("GET", "/sp/spawn?prog=" + p, None) for p in ("pwd", "nosuchcmd-p2a", "/nonexistent/x", "/tmp", "/etc/hosts",
                                                         "./nosuch")]
STEPS += [("GET", "/sp/spawn?prog=pwd&cwd=" + d, None) for d in ("/", "/nonexistent", "/etc/hosts")]
STEPS += [("GET", f"/sp/env/{v}", None) for v in ("v", "%C3%A9", "a%20b")]
STEPS += [("GET", f"/sp/timeout/{h}", None) for h in ("kill", "terminate", "2")]
STEPS += [("GET", "/sp/twice", None), ("GET", "/sp/repr", None), ("GET", "/sp/locale", None)]
STEPS += [("GET", f"/sp/refused/{w}", None) for w in ("int", "shell", "text")]
STEPS += [("GET", f"/sp/nul/{w}", None) for w in ("arg", "prog", "env_eq", "env_nul", "env_int", "cwd")]


def reset(db: str) -> None:
    pass
