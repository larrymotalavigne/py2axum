"""loguru's `logger` as applications use it (dynrt/loguru.rs): the default handler on sys.stderr, the level methods
and `log()` with their call site (module, function, line), `str.format` of the message, sinks added on sys.stdout
with a format of their own (colours included) and removed, loguru's errors. No database."""
import sys

from fastapi import FastAPI
from loguru import logger

from .helpers import Box, Service, nested_log

# at import, as an application announces itself and adds its own sink
logger.info("loguruapp imported")
STDOUT_ID = logger.add(sys.stdout, level="INFO",
                       format="{time:YYYY-MM-DD HH:mm:ss.SSS} [{level.no:>3}] {module}|{file}|{function}:{line} "
                              "<red>{message}</red> {{literal}} {level.name}")

app = FastAPI()


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/levels")
async def levels():
    logger.trace("trace is below the default handler's DEBUG")
    logger.debug("debug message")
    logger.info("info message")
    logger.success("success message")
    logger.warning("warning message")
    logger.error("error message")
    logger.critical("critical message")
    return {"ok": True}


@app.get("/format/{name}")
async def fmt(name: str, n: int = 3):
    logger.info("hello {} n={n}", name, n=n)
    logger.info("no arguments: braces {kept} {}")
    logger.info(f"f-string {name!r} {n * 2}")
    logger.info("{0}-{0}-{1:>5}|", name, n)
    logger.warning(
        "multi-line call for {}",
        name,
    )
    logger.info(12345)
    logger.info(Box(name))
    return {"name": name}


@app.get("/log/{level}")
async def log_level(level: str):
    out = {}
    for lv in (level, "INFO", 25, 7, 55, True):
        try:
            logger.log(lv, "log() with {!r}", lv)
            out[str(lv)] = "ok"
        except (ValueError, TypeError) as e:
            out[str(lv)] = f"{type(e).__name__}: {e}"
    return out


@app.get("/errors")
async def errors():
    out = {}
    cases = [lambda: logger.log(1.5, "float level"), lambda: logger.log(-3, "negative"),
             lambda: logger.log("NOPE", "unknown"), lambda: logger.info("missing {} {}", 1),
             lambda: logger.info("missing {key}", other=1), lambda: logger.info(42, 1),
             lambda: logger.remove(10_000), lambda: logger.remove("x"), lambda: logger.info(),
             lambda: logger.log("INFO")]
    for i, f in enumerate(cases):
        try:
            f()
            out[i] = "ok"
        except Exception as e:  # noqa: BLE001
            out[i] = f"{type(e).__name__}: {e}"
    return out


@app.get("/where")
def where():
    nested_log("sync endpoint")
    Service().run("method")
    (lambda: logger.info("from a lambda"))()
    return [x for x in range(2) if logger.debug("in a comprehension {}", x) is None]


@app.post("/sinks")
async def sinks(colorize: bool = False, level: str = "DEBUG"):
    hid = logger.add(sys.stdout, level=level, colorize=colorize,
                     format="<green>{level: <8}</green> <level>{message}</level> | <cyan>{name}</cyan>:{function} "
                            "<b><u>bold</u> <fg #ff8800>orange</fg #ff8800> <bg 21>bg</bg 21></b> \\<tag> {time:HH:mm:ss}")
    logger.debug("to {} handlers", "several")
    logger.info("info to several")
    logger.error("error to several")
    logger.log(33, "custom level number")
    logger.remove(hid)
    logger.info("after remove({})", hid)
    return {"id": hid}


@app.post("/quiet")
async def quiet():
    # every handler removed, then the defaults back: the stderr sink again, and the stdout one
    global STDOUT_ID
    logger.remove()
    logger.error("nobody hears this")
    logger.log("NOPE", "no handler: the level is not even checked")
    first = logger.add(sys.stderr)
    STDOUT_ID = logger.add(sys.stdout, level=20, colorize=False,
                           format="{time:YYYY-MM-DD HH:mm:ss.SSS} [{level.no:>3}] {module}|{file}|{function}:{line} "
                                  "<red>{message}</red> {{literal}} {level.name}")
    logger.info("handlers back: {} and {}", first, STDOUT_ID)
    return {"first": first, "stdout": STDOUT_ID}
