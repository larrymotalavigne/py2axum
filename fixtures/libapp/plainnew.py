"""A plain class's `__new__`: a factory returning a class attribute (`__init__` never runs), a singleton through
`super().__new__(cls)` / `object.__new__(cls)`, `__init__` after `__new__` on an instance, inherited `__new__`."""
from fastapi import APIRouter

router = APIRouter(prefix="/plainnew")


class Grade:
    LOW = "LOW"
    MID = "MID"
    HIGH = "HIGH"
    NONE = "NONE"

    def __new__(cls, score):
        match score:
            case None:
                return cls.NONE
            case s if s in [1, 2, 3]:
                return cls.LOW
            case s if s in [4, 5]:
                return cls.MID
            case _:
                return cls.HIGH


class Registry:
    _instance = None
    created = 0

    def __new__(cls, *args, **kwargs):
        if cls._instance is None:
            cls._instance = super().__new__(cls)
        return cls._instance

    def __init__(self, name="default"):
        Registry.created += 1
        self.name = name


class Point:
    def __new__(cls, *args, **kwargs):
        obj = object.__new__(cls)
        obj.made_by_new = True
        return obj

    def __init__(self, x, y=0):
        self.x, self.y = x, y


class Point3(Point):
    def __init__(self, x, y=0, z=0):
        super().__init__(x, y)
        self.z = z


class Shy:
    def __new__(cls, n):
        return n * 2

    def __init__(self, n):
        raise RuntimeError("never called")


@router.get("/run")
async def run():
    a = Registry("first")
    b = Registry("second")
    p = Point(1, y=2)
    q = Point3(3, 4, 5)
    kind = Grade
    return {
        "grades": [Grade(s) for s in (None, 1, 3, 4, 5, 6, 100)],
        "held": kind(2),
        "types": [type(Grade(1)).__name__, isinstance(Grade(4), str)],
        "singleton": [a is b, a.name, Registry.created],
        "point": [p.x, p.y, p.made_by_new, type(p).__name__],
        "point3": [q.x, q.y, q.z, q.made_by_new, isinstance(q, Point)],
        "shy": Shy(21),
    }
