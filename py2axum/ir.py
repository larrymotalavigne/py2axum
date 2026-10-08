"""What the translator shares: the error type, and the GZipMiddleware options read by the front-end."""
from __future__ import annotations

import ast
from dataclasses import dataclass


class TranspileError(Exception):
    """A construct outside the supported subset. Always points at a source line."""

    def __init__(self, msg: str, node: ast.AST | None = None, file: str | None = None):
        self.msg = msg
        self.node = node
        self.file = file
        super().__init__(self.render())

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
