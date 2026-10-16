"""email.message.EmailMessage under the modern policies: headers set then read back (unstructured and address
headers: parse, decoded value), set_content(str) (its Content-Transfer-Encoding), as_bytes()/as_string()/str()/
bytes() (header folding, encoded words), the errors; the cases come from tests/scenarios/libapp.py."""
import email.message
import email.policy
from email.errors import HeaderWriteError, MessageError
from email.message import EmailMessage

from fastapi import APIRouter

router = APIRouter(prefix="/emailx")
POLICIES = {"default": email.policy.default, "SMTP": email.policy.SMTP, "SMTPUTF8": email.policy.SMTPUTF8,
            "HTTP": email.policy.HTTP}


def _err(e: Exception) -> str:
    return f"{type(e).__name__}: {e}"


@router.post("/run")
async def run(case: dict):
    out = {}
    try:
        pol = case.get("policy")
        msg = EmailMessage(policy=POLICIES[pol]) if pol else EmailMessage()
        for k, v in case.get("before", []):
            msg[k] = v
        if "body" in case:
            kw = case.get("kw", {})
            if "subtype" in kw and "cte" in kw:
                msg.set_content(case["body"], subtype=kw["subtype"], cte=kw["cte"])
            elif "subtype" in kw:
                msg.set_content(case["body"], subtype=kw["subtype"])
            elif "cte" in kw:
                msg.set_content(case["body"], cte=kw["cte"])
            elif "charset" in kw:
                msg.set_content(case["body"], charset=kw["charset"])
            else:
                msg.set_content(case["body"])
        for k, v in case.get("after", []):
            msg[k] = v
        out["get"] = [[k, str(msg[k]) if msg[k] is not None else None] for k, _ in case.get("before", []) + case.get("after", [])]
        try:
            out["bytes"] = msg.as_bytes().decode("latin-1")
        except Exception as e:
            out["bytes_exc"] = _err(e)
        try:
            out["string"] = msg.as_string()
        except Exception as e:
            out["string_exc"] = _err(e)
        if case.get("str"):
            try:
                out["str"] = str(msg)
            except Exception as e:
                out["str_exc"] = _err(e)
            out["dunder_bytes"] = bytes(msg).decode("latin-1")
    except Exception as e:
        out["exc"] = _err(e)
    return out


@router.get("/api")
async def api():
    """The mapping-like methods, and the message kept by an error half way through set_content."""
    msg = email.message.EmailMessage()
    msg["To"] = "a@example.com"
    msg["X-Tag"] = "one"
    msg["x-tag"] = "two"
    msg["Cc"] = ["b@example.com", "Bé <c@example.com>"]
    msg["Bcc"] = None
    msg["Subject"] = None
    out = {"keys": msg.keys(), "values": msg.values(), "items": [list(x) for x in msg.items()], "len": len(msg),
           "in": ["to" in msg, "X-TAG" in msg, "Reply-To" in msg], "get": msg.get("x-tag"), "missing": msg.get("nope"),
           "default": msg.get("nope", "dflt"), "all": msg.get_all("X-Tag"), "none_all": msg.get_all("nope"),
           "iter": [k for k in msg], "bool": bool(msg), "empty_bool": bool(EmailMessage())}
    del msg["x-tag"]
    out["after_del"] = msg.keys()
    try:
        msg.set_content("é", charset="ascii")
    except UnicodeEncodeError as e:
        out["encode_err"] = _err(e)
    out["half"] = msg.as_string()
    try:
        msg.set_content("x", cte="BASE64")
    except ValueError as e:
        out["cte_err"] = _err(e)
    msg.set_content("first")
    msg.set_content("second", subtype="html")
    out["twice"] = msg.as_string()
    for bad in ("Bad Name", "Bad:Name", "Bé", "", "X-Ok\n"):
        try:
            m = EmailMessage()
            m[bad] = "v"
            out.setdefault("names", []).append([bad, m.as_string()])
        except ValueError as e:
            out.setdefault("names", []).append([bad, _err(e)])
    errs = [issubclass(HeaderWriteError, MessageError), isinstance(HeaderWriteError("x"), Exception)]
    out["errs"] = errs
    return out


@router.get("/policies")
async def policies(policy: str, how: str):
    msg = EmailMessage(policy=POLICIES[policy])
    msg["From"] = "Jöhn Doé <john@example.com>"
    msg["Subject"] = "Réunion " * 12
    try:
        # HTTP's unlimited line length: a TypeError before 3.13.10 / 3.14.1
        msg.set_content("line é\n" * 3 + "x" * 100 + "\n")
    except TypeError as e:
        return {"err": _err(e)}
    if how == "bytes":
        return {"v": msg.as_bytes().decode("latin-1")}
    if how == "string":
        return {"v": msg.as_string()}
    if how == "str":
        return {"v": str(msg)}
    if how == "as_smtp":
        return {"v": msg.as_bytes(policy=email.policy.SMTP).decode("latin-1")}
    return {"v": msg.as_string(policy=email.policy.SMTPUTF8)}
