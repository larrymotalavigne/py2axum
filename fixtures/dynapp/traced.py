"""Functions observed by fixtures/dynapp/tracing.py (single-line statements, no suspension point: CPython
reports each `await` that suspends as a return and a new call)."""


class Shelf:
    def __init__(self, items: list):
        self.items = items

    def first(self):
        return self.items[0]


def lookup(table: dict, key: str):
    return table[key]


def deeper(table: dict, key: str):
    return lookup(table, key)


def tolerant(table: dict, key: str):
    try:
        return lookup(table, key)
    except KeyError:
        return None


async def compute(n: int) -> int:
    return double(n) + 1


def double(n: int) -> int:
    return n * 2


async def failing() -> None:
    deeper({}, "missing")


twice = lambda n: n * 2  # noqa: E731
