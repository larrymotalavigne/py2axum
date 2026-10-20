"""Generators that call generators: an async generator iterating another, a method generator, a generator handed
to an `@asynccontextmanager`, `yield from` a list in a streamed generator. A generator function that does not
translate (its stub keeps the yielder parameter its callers pass: the crate compiled with E0061 before) is in
tests/test_generated_compiles.py."""
from contextlib import asynccontextmanager

from fastapi import APIRouter
from fastapi.responses import StreamingResponse

router = APIRouter(prefix="/gens")


async def count(n: int):
    for i in range(n):
        yield i


async def doubled(n: int):
    async for i in count(n):
        if i == 3:
            continue
        yield i * 2


class Bag:
    def __init__(self, items: list[str]):
        self.items = items

    async def upper(self):
        for s in self.items:
            yield s.upper()

    async def pairs(self):
        async for s in self.upper():
            yield s + s


@asynccontextmanager
async def drained(gen):
    seen = []
    try:
        yield seen
    finally:
        async for x in gen:
            seen.append(x)


def lines(words: list[str]):
    yield "start\n"
    yield from sorted(w + "\n" for w in words)
    yield "end\n"


async def streamed(n: int):
    async for i in doubled(n):
        yield f"{i},"


@router.get("/doubled/{n}")
async def get_doubled(n: int):
    return [i async for i in doubled(n)]


@router.get("/bag")
async def get_bag(w: str = "a,b"):
    bag = Bag(w.split(","))
    return {"upper": [s async for s in bag.upper()], "pairs": [s async for s in bag.pairs()]}


@router.get("/drained/{n}")
async def get_drained(n: int):
    async with drained(count(n)) as seen:
        before = list(seen)
    return {"before": before, "after": seen}


@router.get("/lines")
async def get_lines(w: str = "x,y"):
    return StreamingResponse(lines(w.split(",")), media_type="text/plain")


@router.get("/streamed/{n}")
async def get_streamed(n: int):
    return StreamingResponse(streamed(n), media_type="text/plain")
