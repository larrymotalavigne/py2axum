"""Router registration forms of real apps (P2A0304 of the app bench). Translated as `fixtures/routerapp/web`: the
routers of `extra/` sit outside the translated directory and are reached only through the app's imports (src/
layout); `web/optional.py` does not exist, so its import guard falls back and what it guards never runs. No database."""
