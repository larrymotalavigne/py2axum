import enum


class Priority(str, enum.Enum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"


class Status(enum.Enum):
    OPEN = "open"
    DONE = "done"


class Channel(enum.StrEnum):
    MAIL = "mail"
    SMS = "sms"


class Level(enum.IntEnum):
    ONE = 1
    TWO = 2


class Group(enum.Enum):
    """List and tuple values: a plain enum keeps them as they are."""
    TEXT = ["street", "city"]
    GEO = ["lat", "lon"]
    PAIR = (1, "a")


class Relation(str, enum.Enum):
    """A str mixin calls str(*value): a one-element tuple is its string."""
    MASTER = ("MASTER",)
    CHILD = ("CHILD",)
