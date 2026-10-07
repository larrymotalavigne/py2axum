"""tenacity: retried calls (attempt counting, reraise, RetryError, callbacks, strategies)."""
import inspect
import logging

from fastapi import APIRouter
from tenacity import (
    RetryError,
    before_sleep_log,
    retry,
    retry_if_exception,
    retry_if_exception_type,
    retry_if_result,
    stop_after_attempt,
    stop_after_delay,
    wait_exponential_jitter,
    wait_fixed,
    wait_random,
)

router = APIRouter(prefix="/retry")
logger = logging.getLogger("fixtures.retry")
CALLS: dict[str, int] = {}


def hit(name: str) -> int:
    CALLS[name] = CALLS.get(name, 0) + 1
    return CALLS[name]


@retry(stop=stop_after_attempt(3), wait=wait_fixed(0), before_sleep=before_sleep_log(logger, logging.WARNING))
async def flaky(fail_times: int) -> str:
    """Fails `fail_times` times."""
    n = hit("flaky")
    if n <= fail_times:
        raise ConnectionError(f"attempt {n}")
    return f"ok after {n}"


@retry(stop=stop_after_attempt(2), wait=wait_exponential_jitter(initial=0.001, max=0.002), reraise=True)
async def always_reraise() -> None:
    hit("reraise")
    raise ValueError("boom")


@retry(stop=stop_after_attempt(2) | stop_after_delay(30), wait=wait_fixed(0) + wait_random(0, 0))
async def always_wrapped() -> None:
    hit("wrapped")
    raise KeyError("k")


def on_give_up(state):
    return {"attempts": state.attempt_number, "failed": state.outcome.failed,
            "exc": repr(state.outcome.exception()), "fn": state.fn.__name__}


@retry(stop=stop_after_attempt(3), retry_error_callback=on_give_up)
async def with_callback(x: int):
    hit("callback")
    raise RuntimeError(f"x={x}")


@retry(retry=retry_if_exception_type(ValueError), stop=stop_after_attempt(4))
async def only_value_errors(kind: str):
    hit("typed")
    raise (ValueError if kind == "value" else TypeError)(kind)


@retry(retry=retry_if_exception(lambda e: "again" in str(e)), stop=stop_after_attempt(3), reraise=True)
async def predicate(msg: str):
    hit("pred")
    raise OSError(msg)


@retry(retry=retry_if_result(lambda r: r is None), stop=stop_after_attempt(3), retry_error_callback=lambda s: "gave up")
async def until_value(after: int):
    n = hit("result")
    return n if n > after else None


@retry(stop=stop_after_attempt(2))
def sync_flaky() -> int:
    n = hit("sync")
    if n < 2:
        raise ConnectionError("sync")
    return n


@retry
async def bare() -> str:
    hit("bare")
    return "bare"


async def run(coro_fn, *args):
    try:
        return {"value": await coro_fn(*args)}
    except RetryError as e:
        return {"error": "RetryError", "last_failed": e.last_attempt.failed,
                "last": repr(e.last_attempt.exception())}
    except Exception as e:
        return {"error": type(e).__name__, "msg": str(e)}


@router.get("/case/{name}")
async def case(name: str, arg: int = 0):
    CALLS.clear()
    if name == "flaky":
        out = await run(flaky, arg)
    elif name == "reraise":
        out = await run(always_reraise)
    elif name == "wrapped":
        out = await run(always_wrapped)
    elif name == "callback":
        out = await run(with_callback, arg)
    elif name == "typed-value":
        out = await run(only_value_errors, "value")
    elif name == "typed-other":
        out = await run(only_value_errors, "other")
    elif name == "pred-again":
        out = await run(predicate, "try again")
    elif name == "pred-stop":
        out = await run(predicate, "fatal")
    elif name == "result":
        out = await run(until_value, arg)
    elif name == "sync":
        try:
            out = {"value": sync_flaky()}
        except Exception as e:
            out = {"error": type(e).__name__}
    else:
        out = await run(bare)
    return {"out": out, "calls": CALLS, "meta": [flaky.__name__, flaky.__doc__, inspect.iscoroutinefunction(flaky),
                                                 inspect.iscoroutinefunction(sync_flaky), sync_flaky.__wrapped__.__name__]}
