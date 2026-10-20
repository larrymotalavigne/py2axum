"""A base class in its own module: the subclasses of klass.py inherit its `__init__` and classmethod."""
from typing import Annotated

from fastapi import Header, Request


class RemoteBase:
    def __init__(self, request: Request, x_token: Annotated[str | None, Header()] = None, limit: int = 5):
        self.method = request.method
        self.token = x_token
        self.limit = limit

    @classmethod
    async def build(cls, request: Request, limit: int = 2):
        return cls(request, "from-build", limit)

    def out(self):
        return {"cls": type(self).__name__, "method": self.method, "token": self.token, "limit": self.limit}
