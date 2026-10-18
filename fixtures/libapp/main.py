"""Reference app of the popular libraries of 0.8 (orjson, PyYAML, croniter, Unidecode, aio-pika, ecs-logging) and of a
plain class's __new__ (then itsdangerous URLSafeSerializer and mimetypes, email.message.EmailMessage and dnspython, pytz,
dateutil's parser and tz, cachetools, 0.9),
apart from fixtures/dynapp: with their generated case tables, dynapp's crate no longer compiled within the CI build
containers' 6 GiB (rustc's front-end alone)."""
from fastapi import FastAPI

from . import amqpx, cronx, ctoolsx, dnsx, duparsex, dutzx, ecslogs, emailx, itsx, mimex, orjsonx, plainnew, pytzx, unidec, yamlx

app = FastAPI()
app.include_router(orjsonx.router)
app.include_router(yamlx.router)
app.include_router(cronx.router)
app.include_router(unidec.router)
app.include_router(amqpx.router)
app.include_router(ecslogs.router)
app.include_router(plainnew.router)
app.include_router(itsx.router)
app.include_router(mimex.router)
app.include_router(emailx.router)
app.include_router(dnsx.router)
app.include_router(pytzx.router)
app.include_router(dutzx.router)
app.include_router(duparsex.router)
app.include_router(ctoolsx.router)


@app.get("/health")
async def health():
    return {"ok": True}
