"""asyncio.create_subprocess_exec (dynrt/subproc.rs): pipes, communicate, return codes, signals, timeouts and the
OSError of a failed spawn (named file under asyncio, not under uvloop). Commands of POSIX systems only (sh, printf,
cat, head, sleep, pwd). No database."""
import asyncio
import os
import subprocess
from asyncio.subprocess import DEVNULL, PIPE, STDOUT

from fastapi import FastAPI, Request

app = FastAPI()


def oserror(e: OSError) -> dict:
    return {"type": type(e).__name__, "str": str(e), "errno": e.errno, "strerror": e.strerror, "filename": e.filename}


def text(b):
    return None if b is None else b.decode("utf-8", "replace")


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/sp/printf")
async def sp_printf(fmt: str = "%s", arg: str = "hi"):
    # returncode before communicate() is left out: a fast child may already have exited (0) or not (None)
    p = await asyncio.create_subprocess_exec("printf", fmt, arg, stdout=PIPE)
    out, err = await p.communicate()
    return {"out": text(out), "err": err, "rc": p.returncode, "pid": isinstance(p.pid, int)}


@app.get("/sp/star/{a}/{b}")
async def sp_star(a: str, b: str):
    cmd = ["printf", "%s-%s", a, b]
    p = await asyncio.create_subprocess_exec(*cmd, stdout=subprocess.PIPE)
    out, _ = await p.communicate()
    return {"out": text(out)}


@app.post("/sp/cat")
async def sp_cat(request: Request, keyword: bool = False):
    body = await request.body()
    p = await asyncio.create_subprocess_exec("cat", stdin=PIPE, stdout=PIPE)
    out, err = await (p.communicate(input=body) if keyword else p.communicate(body))
    return {"len": len(out), "same": out == body, "head": text(out[:40]), "rc": await p.wait()}


@app.get("/sp/big/{n}")
async def sp_big(n: int):
    p = await asyncio.create_subprocess_exec("head", "-c", str(n), "/dev/zero", stdout=PIPE, stderr=PIPE)
    out, err = await p.communicate()
    return {"len": len(out), "err": text(err), "rc": p.returncode}


@app.get("/sp/sh")
async def sp_sh(code: int = 0, merge: bool = False, devnull: bool = False):
    script = f"echo out; echo err >&2; exit {code}"
    if devnull:
        p = await asyncio.create_subprocess_exec("sh", "-c", script, stdout=DEVNULL, stderr=DEVNULL)
    elif merge:
        p = await asyncio.create_subprocess_exec("sh", "-c", script, stdout=PIPE, stderr=STDOUT)
    else:
        p = await asyncio.create_subprocess_exec("sh", "-c", script, stdout=PIPE, stderr=PIPE)
    out, err = await p.communicate()
    return {"out": text(out), "err": text(err), "rc": p.returncode}


@app.get("/sp/spawn")
async def sp_spawn(prog: str, cwd: str | None = None):
    try:
        p = await asyncio.create_subprocess_exec(prog, stdout=PIPE, cwd=cwd)
    except OSError as e:
        return oserror(e)
    out, _ = await p.communicate()
    return {"out": text(out), "rc": p.returncode}


@app.get("/sp/env/{value}")
async def sp_env(value: str):
    p = await asyncio.create_subprocess_exec("sh", "-c", 'printf "%s|%s" "$P2A_X" "$HOME"', stdout=PIPE,
                                             env={"P2A_X": value, "PATH": "/usr/bin:/bin"})
    out, _ = await p.communicate()
    return {"out": text(out)}


@app.get("/sp/timeout/{how}")
async def sp_timeout(how: str):
    p = await asyncio.create_subprocess_exec("sleep", "30", stdout=PIPE)
    try:
        await asyncio.wait_for(p.communicate(), 0.3)
        timed_out = False
    except TimeoutError:
        timed_out = True
    running = p.returncode
    if how == "kill":
        p.kill()
    elif how == "terminate":
        p.terminate()
    else:
        p.send_signal(int(how))
    rc = await p.wait()
    try:
        p.kill()
        again = None
    except ProcessLookupError as e:
        again = [type(e).__name__, str(e), isinstance(e, OSError)]
    return {"timed_out": timed_out, "running": running, "rc": rc, "again": again}


@app.get("/sp/twice")
async def sp_twice():
    p = await asyncio.create_subprocess_exec("printf", "x", stdout=PIPE)
    first = await p.communicate()
    second = await p.communicate()
    return {"first": [text(x) for x in first], "second": [text(x) for x in second], "rc": await p.wait()}


@app.get("/sp/refused/{what}")
async def sp_refused(what: str):
    try:
        if what == "int":
            await asyncio.create_subprocess_exec("printf", 3, stdout=PIPE)
        elif what == "shell":
            await asyncio.create_subprocess_exec("printf", "x", shell=True)
        else:
            await asyncio.create_subprocess_exec("printf", "x", text=True)
    except (TypeError, ValueError) as e:
        return {"type": type(e).__name__, "str": str(e)}
    return {"type": None}


@app.get("/sp/repr")
async def sp_repr():
    p = await asyncio.create_subprocess_exec("printf", "", stdout=PIPE)
    await p.communicate()
    return {"repr": repr(p) == f"<Process {p.pid}>", "str": str(p) == repr(p), "stdin": p.stdin}


@app.get("/sp/locale")
async def sp_locale():
    # CPython coerces the C locale at startup (PEP 538): LC_CTYPE=C.UTF-8 in os.environ and in the children
    p = await asyncio.create_subprocess_exec("sh", "-c", 'printf "%s|%s" "${LC_CTYPE-unset}" "${LC_ALL-unset}"', stdout=PIPE)
    out, _ = await p.communicate()
    return {"environ": os.environ.get("LC_CTYPE"), "child": text(out)}


@app.get("/sp/nul/{what}")
async def sp_nul(what: str):
    # NUL bytes and illegal names: ValueError under asyncio; uvloop passes C strings (the value ends at the NUL)
    cases = {"arg": (["printf", "%s", "a\x00b"], {}), "prog": (["pri\x00ntf", "x"], {}),
             "env_eq": (["sh", "-c", "env | grep -c '^A=B' || true"], {"env": {"A=B": "1", "PATH": "/usr/bin:/bin"}}),
             "env_nul": (["sh", "-c", 'printf %s "$A"'], {"env": {"A": "1\x002", "PATH": "/usr/bin:/bin"}}),
             "env_int": (["true"], {"env": {"A": 1}}), "cwd": (["pwd"], {"cwd": "/\x00tmp"})}
    args, kw = cases[what]
    try:
        p = await asyncio.create_subprocess_exec(*args, stdout=PIPE, **kw)
    except (OSError, ValueError, TypeError) as e:
        return {"type": type(e).__name__, "str": str(e)}
    out, _ = await p.communicate()
    return {"out": text(out), "rc": p.returncode}
