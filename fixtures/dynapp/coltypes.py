from sqlalchemy import Numeric

# a money column: exact NUMERIC storage, read and written as float
Money = Numeric(precision=10, scale=2, asdecimal=False)


# an encrypted string column: Fernet at rest behind an `enc:v1:` prefix
import os  # noqa: E402
from functools import lru_cache  # noqa: E402

from cryptography.fernet import Fernet, InvalidToken  # noqa: E402
from sqlalchemy import String  # noqa: E402
from sqlalchemy.types import TypeDecorator  # noqa: E402

PREFIX = "enc:v1:"


@lru_cache(maxsize=1)
def _fernet() -> Fernet | None:
    key = (os.getenv("DYNAPP_FERNET_KEY") or "M2ZhYmZkZjA5YTQ0NGQ1ZGIzN2Y3ZWM5MzFjZmQxNDY=").strip()
    if not key:
        return None
    try:
        return Fernet(key.encode())
    except (ValueError, TypeError):
        return None


def encrypt_value(value: str | None) -> str | None:
    if value is None or value == "" or value.startswith(PREFIX):
        return value
    cipher = _fernet()
    if cipher is None:
        return value
    return PREFIX + cipher.encrypt(value.encode()).decode()


def decrypt_value(value: str | None) -> str | None:
    if value is None or not value.startswith(PREFIX):
        return value
    cipher = _fernet()
    if cipher is None:
        return None
    try:
        return cipher.decrypt(value[len(PREFIX):].encode()).decode()
    except InvalidToken:
        return None


class EncryptedString(TypeDecorator[str]):
    """String encrypted at rest."""

    impl = String
    cache_ok = True

    def process_bind_param(self, value, dialect):  # noqa: ANN001
        return encrypt_value(value)

    def process_result_value(self, value, dialect):  # noqa: ANN001
        return decrypt_value(value)
