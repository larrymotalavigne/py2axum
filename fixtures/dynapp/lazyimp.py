"""importlib.import_module on a literal project module: attributes read by name at run time."""
from fastapi import APIRouter
from pydantic import TypeAdapter

router = APIRouter(prefix="/lazy")


class LazyRef:
    __slots__ = ("attr",)

    def __init__(self, attr: str):
        self.attr = attr

    def resolve(self):
        from importlib import import_module

        return getattr(import_module("fixtures.dynapp.lazydata"), self.attr)


@router.get("/attr/{name}")
async def attr(name: str):
    import importlib

    mod = importlib.import_module("fixtures.dynapp.lazydata")
    try:
        value = LazyRef(name).resolve()
    except AttributeError as e:
        return {"error": str(e), "has": hasattr(mod, name)}
    return {"value": value if not callable(value) else "callable", "has": hasattr(mod, name),
            "dflt": getattr(mod, "nope", "fallback"), "mod": mod.__name__}


@router.get("/use")
async def use():
    import importlib

    mod = importlib.import_module("fixtures.dynapp.lazydata")
    point = TypeAdapter(mod.Point).validate_python(mod.DEFAULT_POINT)
    return {"scaled": mod.scale(2), "scaled3": mod.scale(2, by=3), "point": point.model_dump(),
            "fn": getattr(mod, "scale")(1)}
