"""Pure-Python modules of the standard library translated from the project's Python (Lib/): html.entities, gettext,
colorsys, keyword. (bisect is not: CPython runs its C accelerator _bisect, see libsrc.STDLIB_SOURCE.)"""
import colorsys
import gettext
import keyword
from html.entities import codepoint2name, html5, name2codepoint

from fastapi import APIRouter

router = APIRouter(prefix="/std")


@router.get("/entity/{name}")
async def entity(name: str):
    cp = name2codepoint.get(name)
    return {"cp": cp, "back": codepoint2name.get(cp) if cp is not None else None, "html5": html5.get(name + ";")}


@router.get("/gettext/{msg}")
async def tr(msg: str, fallback: bool = False):
    # ngettext/npgettext warn through sys._getframe() for a plural that is not an int: refused
    t = gettext.NullTranslations()
    if fallback:
        t.add_fallback(gettext.NullTranslations())
    return {"one": t.gettext(msg), "p": t.pgettext("menu", msg), "info": t.info(), "charset": t.charset()}


@router.get("/color")
async def color(r: float, g: float, b: float):
    return {"hsv": colorsys.rgb_to_hsv(r, g, b), "hls": colorsys.rgb_to_hls(r, g, b), "yiq": colorsys.rgb_to_yiq(r, g, b),
            "back": colorsys.hsv_to_rgb(*colorsys.rgb_to_hsv(r, g, b))}


@router.get("/kw/{w}")
async def kw(w: str):
    return {"kw": keyword.iskeyword(w), "soft": keyword.issoftkeyword(w)}
