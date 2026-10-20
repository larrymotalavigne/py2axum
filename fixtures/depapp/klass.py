"""Class dependencies whose `__init__` is inherited, classmethods and staticmethods as dependencies."""
import abc
from typing import Annotated

from fastapi import APIRouter, Depends, Request

from .remote import RemoteBase

from .state import EVENTS

router = APIRouter(prefix="/k")


class BaseService(metaclass=abc.ABCMeta):
    @abc.abstractmethod
    def describe(self) -> str: ...


class Pager:
    def __init__(self, page: int = 1, size: int = 10):
        self.page = page
        self.size = size


class Pages(Pager):
    """No `__init__`: FastAPI reads Pager's."""

    def window(self):
        return [self.page * self.size, (self.page + 1) * self.size]


class Service(BaseService):
    def __init__(self, request: Request, pager: Annotated[Pages, Depends()], tag: str = "t"):
        self.path = request.url.path
        self.pager = pager
        self.tag = tag
        EVENTS.append(f"init {type(self).__name__} {tag}")

    def describe(self) -> str:
        return f"{type(self).__name__} {self.path} {self.tag} {self.pager.window()}"

    @classmethod
    async def async_init(cls, request: Request, pager: Annotated[Pages, Depends()], tag: str = "c"):
        return cls(request, pager, tag)

    @classmethod
    def sync_init(cls, request: Request, size: int = 3):
        return cls(request, Pages(2, size), "sync")

    @staticmethod
    def static_make(flag: bool = False) -> dict:
        return {"flag": flag}


class ServiceAPI(Service):
    def describe(self) -> str:
        return "api: " + Service.describe(self)


class Unfinished(BaseService):
    """Abstract method not implemented: calling the class is CPython's TypeError (500)."""

    def __init__(self, n: int = 0):
        self.n = n


@router.get("/pages")
async def pages(p: Annotated[Pages, Depends()]):
    return {"window": p.window()}


@router.get("/pages2")
async def pages2(p: Pages = Depends(Pages)):
    return {"window": p.window(), "page": p.page}


@router.get("/service")
async def service(s: Annotated[Service, Depends()]):
    return {"d": s.describe()}


@router.get("/service/api")
async def service_api(s: ServiceAPI = Depends(ServiceAPI)):
    return {"d": s.describe()}


@router.get("/cm")
async def cm(s: Annotated[Service, Depends(Service.async_init)]):
    return {"d": s.describe()}


@router.get("/cm/api")
async def cm_api(s: Annotated[ServiceAPI, Depends(ServiceAPI.async_init)]):
    return {"d": s.describe()}


@router.get("/cm/twice")
async def cm_twice(a: Annotated[Service, Depends(Service.async_init)],
                   b: Annotated[Service, Depends(Service.async_init)]):
    return {"same": a is b, "d": a.describe()}


@router.get("/cm/sync")
def cm_sync(s: Service = Depends(Service.sync_init)):
    return {"d": s.describe()}


@router.get("/static", dependencies=[Depends(ServiceAPI.async_init)])
def static(v: dict = Depends(Service.static_make)):
    return v


@router.get("/unfinished")
def unfinished(u: Unfinished = Depends()):
    return {"n": u.n}


class Remote(RemoteBase):
    """`__init__` and `build` inherited from another module, whose signature reads names this one does not import."""


@router.get("/remote")
def remote(r: Annotated[Remote, Depends()]):
    return r.out()


@router.get("/remote/build")
async def remote_build(r: Remote = Depends(Remote.build)):
    return r.out()
