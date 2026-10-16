"""Imported with `from .starexp import *` by fixtures/langapp/construct.py: without `__all__`, every name not starting
with `_`, the imported ones included (HTTPException)."""
from fastapi import HTTPException  # noqa: F401

STAR_DETAIL = "raised through a star import"
_HIDDEN = "not exported"


def exported() -> str:
    return "exported"
