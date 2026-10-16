"""itsdangerous.URLSafeSerializer (no timestamp): deterministic tokens compared byte for byte, key rotation, salts,
tampered tokens, payloads signed by the real Signer that do not decode, and timed/untimed tokens mixed up."""
from fastapi import APIRouter
from itsdangerous import BadData, BadPayload, BadSignature, URLSafeSerializer, URLSafeTimedSerializer

router = APIRouter(prefix="/itsx")
SECRET = "lib-secret"
# itsdangerous.Signer("lib-secret", salt="itsx").sign(...) of payloads the serializer cannot load (2.2.0)
SIGNED_GARBAGE = {
    "b64": "!!!.fDNK_JbpRTKWRWhbK7sIRlbpQ4M",
    "zlib": ".AAAA.eKuOgjwuWE2MTKhD4Xj7-s_Fvjs",
    "json": "bm90IGpzb24.hw0gLhIIUGp0N63LG3_LhvbfxqY",
    "utf8": "_w.dAAqQT653rpGraDTLi8QTN19aio",
    "empty": ".NSMSfQ9XOmxXSKYQHu5LZmpSh9c",
    "cut": "WzEsIDI._aIozBceZSOs349_C1lGEuNQw04",
    "zlib_short": ".eHg.Ma5-QH1PIcUhGQz0-mKZVd-Hng4",
}


def _err(e: Exception) -> dict:
    return {"err": type(e).__name__, "msg": str(e), "payload": getattr(e, "payload", "-") if not isinstance(e, TypeError) else "-",
            "bad_signature": isinstance(e, BadSignature), "bad_data": isinstance(e, BadData),
            "bad_payload": isinstance(e, BadPayload)}


def _serializer(salt: str | None, keys: str) -> URLSafeSerializer:
    key = ["old-key", SECRET] if keys == "rotated" else ["old-key"] if keys == "old" else SECRET
    if salt == "none":
        return URLSafeSerializer(key, salt=None)
    if salt is None:
        return URLSafeSerializer(key)
    return URLSafeSerializer(key, salt)


@router.post("/dumps")
async def dumps(body: dict):
    s = _serializer(body.get("salt"), body.get("keys", "one"))
    try:
        token = s.dumps(body["value"], body["call_salt"]) if "call_salt" in body else s.dumps(body["value"])
    except TypeError as e:
        return _err(e)
    return {"token": token, "type": type(token).__name__, "back": s.loads(token, body.get("call_salt")),
            "cls": type(s).__name__}


@router.get("/loads")
async def loads(token: str, salt: str | None = None, keys: str = "one", call_salt: str | None = None, how: str = "plain"):
    s = _serializer(salt, keys)
    try:
        if how == "positional":
            return {"data": s.loads(token, call_salt)}
        if how == "keyword":
            return {"data": s.loads(token, salt=call_salt)}
        if how == "ignored":
            # Serializer.loads(s, salt=None, **kwargs): the timed serializer's keywords are accepted and ignored
            return {"data": s.loads(token, max_age=-1, return_timestamp=True)}
        if how == "bytes":
            return {"data": s.loads(token.encode())}
        return {"data": s.loads(token)}
    except BadData as e:
        return _err(e)


@router.get("/garbage/{name}")
async def garbage(name: str):
    try:
        return {"data": URLSafeSerializer(SECRET, salt="itsx").loads(SIGNED_GARBAGE[name])}
    except BadPayload as e:
        return _err(e)


@router.get("/tamper")
async def tamper(what: str):
    s = URLSafeSerializer(SECRET)
    token = s.dumps({"id": 7, "u": "unsubscribe"})
    bad = {"sig": token[:-2] + ("AA" if not token.endswith("AA") else "BB"), "nodot": token.replace(".", ""),
           "payload": "x" + token, "sig_b64": token.rsplit(".", 1)[0] + ".!!", "empty": "", "dot": "."}[what]
    try:
        return {"data": s.loads(bad)}
    except BadSignature as e:
        return _err(e)


@router.get("/mixed")
async def mixed(direction: str):
    """a timed token read by the untimed serializer (the signature matches: BadPayload) and the reverse"""
    timed, plain = URLSafeTimedSerializer(SECRET, salt="m"), URLSafeSerializer(SECRET, salt="m")
    try:
        if direction == "timed_to_plain":
            return {"data": plain.loads(timed.dumps([1, 2]))}
        return {"data": timed.loads(plain.dumps([1, 2]))}
    except BadData as e:
        return {"err": type(e).__name__, "msg": str(e)}


@router.get("/errors")
async def errors(what: str):
    s = URLSafeSerializer(SECRET)
    try:
        if what == "set":
            return s.dumps({1, 2})
        if what == "int_token":
            return s.loads(5)
        if what == "too_many":
            return s.loads("a.b", "s", "x")
        if what == "key_int":
            return URLSafeSerializer(5).dumps(1)
    except TypeError as e:
        return {"err": type(e).__name__, "msg": str(e)}
    return {"what": what}
