"""Scenario of fixtures/loguruapp: loguru's records compared on both streams (`--logs`: the default handler on
stderr, the fixture's sinks on stdout, colours included when a sink asks for them). No database."""

STEPS: list = [("GET", "/health", None), ("GET", "/levels", None)]
STEPS += [("GET", f"/format/{n}" + q, None) for n in ("abc", "%7B0%7D", "%C3%A9t%C3%A9") for q in ("", "?n=-12")]
STEPS += [("GET", f"/log/{lv}", None) for lv in ("DEBUG", "TRACE", "SUCCESS", "CRITICAL", "info", "Level%2025", "")]
STEPS += [("GET", "/errors", None), ("GET", "/where", None)]
STEPS += [("POST", "/sinks" + q, None) for q in ("", "?colorize=true", "?colorize=true&level=WARNING", "?level=NOPE",
                                                 "?level=40", "?level=-1")]
STEPS += [("POST", "/quiet", None), ("GET", "/levels", None), ("GET", "/format/x", None), ("POST", "/sinks?colorize=true", None)]


def reset(db: str) -> None:
    pass
