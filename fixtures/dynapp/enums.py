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
