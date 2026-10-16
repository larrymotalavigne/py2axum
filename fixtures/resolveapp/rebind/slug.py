"""Submodule shadowed by the function of the same name that `rebind/__init__.py` imports."""


def slug(text: str) -> str:
    return "-".join(text.lower().split())
