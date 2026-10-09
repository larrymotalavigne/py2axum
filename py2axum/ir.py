"""What the translator shares: the error type, and the GZipMiddleware options read by the front-end."""
from __future__ import annotations

import ast
from dataclasses import dataclass


class TranspileError(Exception):
    """A construct outside the supported subset. Always points at a source line."""

    def __init__(self, msg: str, node: ast.AST | None = None, file: str | None = None, code: str | None = None):
        self.msg = msg
        self.node = node
        self.file = file
        self._code = code
        super().__init__(self.render())

    @property
    def code(self) -> str:
        """Stable code of the error's class (`P2A0501`...): passed at the call site, or found from the message."""
        if self._code:
            return self._code
        from .errors import lookup

        return lookup(self.msg)[0].code

    def explain(self) -> str:
        """The error as the command line prints it: code, file:line, message, why, what to do, link."""
        from .errors import BY_CODE

        c = BY_CODE[self.code]
        return (f"error[{c.code}]: {self.render()}\n"
                f"  = why: {c.why}\n"
                f"  = help: {c.fix}\n"
                f"  = see: {c.anchor()}")

    def render(self) -> str:
        where = ""
        if self.file:
            where = self.file
            if self.node is not None and hasattr(self.node, "lineno"):
                where += f":{self.node.lineno}"
            where += ": "
        return f"{where}{self.msg}"


@dataclass
class Gzip:
    """`app.add_middleware(GZipMiddleware, minimum_size=..., compresslevel=...)`"""

    minimum_size: int = 500  # Starlette defaults
    compresslevel: int = 9
