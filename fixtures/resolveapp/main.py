"""Name resolution and class-level reads that once translated to a wrong value: `from package import name` where the
package's `__init__` rebinds a submodule's name (rebind/), a class held as a class attribute and called through the
class held as a value (`alias = G; alias.NT()`, a stand-in for `gettext` in humanize), methods read on their class
(plain, `@staticmethod`, `@classmethod`). No database."""
from fastapi import FastAPI

from .rebind import plain, slug, starred

app = FastAPI()


@app.get("/health")
async def health():
    return {"ok": True}


# ---------------------------------------------------------------- `from package import name`: the package's attribute


@app.get("/rebind")
async def rebind(text: str = "Hello Big World"):
    return {"slug": slug(text), "starred": starred(2), "plain": plain.helper(), "kind": type(slug).__name__}


# ---------------------------------------------------------------- a class held as a class attribute


class _Translations:
    def __init__(self, lang: str = "en"):
        self.lang = lang

    def gettext(self, s: str) -> str:
        return f"{s}@{self.lang}"


class Gettext:
    NullTranslations = _Translations
    DEFAULT = "en"


TRANSLATIONS = {None: Gettext.NullTranslations()}


@app.get("/class-attr")
async def class_attr(lang: str = "fr"):
    module_alias = Gettext
    return {
        "direct": Gettext.NullTranslations().gettext("a"),
        "alias": module_alias.NullTranslations(lang).gettext("b"),
        "instance": Gettext().NullTranslations(lang).lang,
        "same": module_alias.NullTranslations is _Translations,
        "default": module_alias.DEFAULT,
        "module": TRANSLATIONS[None].gettext("c"),
    }


# ---------------------------------------------------------------- methods read on their class


class Helpers:
    tag = "h"

    def method(self, n: int = 1) -> str:
        return "method" * n

    @staticmethod
    def double(x: int) -> int:
        return x * 2

    @classmethod
    def named(cls, n: int) -> str:
        return cls.__name__ * n + cls.tag


@app.get("/methods")
async def methods(n: int = 2):
    helpers = Helpers
    read = Helpers.method
    static = helpers.double
    return {
        "unbound": [helpers.method(Helpers()), Helpers.method(Helpers(), n), read(Helpers(), n=n)],
        "static": [helpers.double(n), Helpers.double(n), static(n + 1)],
        "classmethod": [helpers.named(n), Helpers.named(1), Helpers().named(1)],
        "types": [type(read).__name__, type(static).__name__, type(helpers.named).__name__],
    }


@app.get("/methods/no-self")
async def methods_no_self():
    read = Helpers.method
    try:
        return {"r": read()}
    except TypeError:
        return {"error": "TypeError"}
