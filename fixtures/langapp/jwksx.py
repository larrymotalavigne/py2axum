"""PyJWT's PyJWKClient against a JWKS endpoint (served by the reference server: both sides fetch the same keys),
PyJWKSet/PyJWK, RS256/PS256 verification. The client's behaviour follows the locked PyJWT (2.13: no refresh
cooldown, redirects followed; 2.14: the cooldown; 2.15: InvalidKeyError for unusable key material, members
that are not objects skipped)."""
import os

import jwt
from fastapi import APIRouter, HTTPException
from jwt import PyJWKClient, PyJWKSet

router = APIRouter(prefix="/jwks")
BASE = os.environ.get("JWKS_BASE", "http://127.0.0.1:8540")

# generated once with PyJWT + cryptography: two RSA keys' public JWKs and tokens signed by them
JWK1 = {'kty': 'RSA', 'key_ops': ['verify'], 'n': '6DRVNag4zKY_2gc61gnoBJhwiIDmR9WaLIl1nc1DLBx1MGUOS-kfgi9qCPHWW0jESPx6iWCDEOXa38MmKqxMDbfCJTpDjY6GkwkHErzf1LxYzVPGVEwYXQscxHro8BBbtlPuzUuvD-lQjcamk7ow8TDbxa9tTQ2qCC4a_ahoaPbjuaf24sGFp0PIA5yoxJL7b5eQoCgZoBHBXHnfGve9ltG5-BSW3g-qRtqF-trAr1pj86KATSRvyvxIS3Ry24eVLN0a0tbb_dSrr07D_8yINAP0sfRjR7X_N9jAf0CxSof2nts51kmcc5NHAF1zo0X1xXgPYAg9ppZublDlrDStBQ', 'e': 'AQAB', 'kid': 'k1', 'use': 'sig', 'alg': 'RS256'}
JWK2 = {'kty': 'RSA', 'key_ops': ['verify'], 'n': 'wkVQHCoYoqzcxxbSbId2se199G4WYIxhzLlnS-HJURFHicF0nCU__zm1aN0E2HsQ9SP8CHa_xLDnbw49EIBmmmdOHw0qwgBIWeI6dgu5xEm3-MViaMW2rw4whem-R1qobt7siXPY58_UVTztZeXXelMHTOzQJVMzzGIto0kq_hWcSpNHaxWB2ho3zheVxO8BDxufoMRYURWUFCa4wkuNMOMw1GXmZ71VcjbPPw9HJVntfB84EizM9A053oo6-xAanBL6VN3R5RtZKl-B5755Gip612em2byLbfcK8ts-4Kuz5_8sw-1YUiYAw2bHBaCNWvkH10Jdq1RDvvk4MK7x_w', 'e': 'AQAB', 'kid': 'k2', 'use': 'sig', 'alg': 'RS256'}
VALID = 'eyJhbGciOiJSUzI1NiIsImtpZCI6ImsxIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJweTJheHVtIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImV4cCI6NDEwMjQ0NDgwMH0.hoGDjnZlc_4a-cYuhGrR9QZCvzMdJQZIWVGzlceh4lN-bWz0xTAiHUn7DSFp01tOwlB4Y8quzexXG04-RgShIu3mqHbBQm3kbKIHrgDCMDzkWEphgzXTKr7V1JJidWnqZ8kNRgRbTI1kUwRrbLgGgfIuuBq2F_a0AuN6E8pPNW70H3OkFJZ6v7YB5yesPH-ujBRx1RVEd7z-x-kxaSutjQh1klSTODL0uj6ZaQ-ZLVyOyb6TU_Vo4FF4NrZxvxkTcOLplGayjxln2Wprpjozf3NVQWsf8sFwrbkuGPJyfPO0XMiqNRTHms8ZfJHvEgpzuDuLYPD5xKc4g4Xz28WOCg'
VALID_PS = 'eyJhbGciOiJQUzI1NiIsImtpZCI6ImsxIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJweTJheHVtIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImV4cCI6NDEwMjQ0NDgwMH0.eUnw_7ELosKgeVbXVZpaBylBaThDwziVS4QKue54XfdljhGAW5Ns_JVJxshxW8k0oiRjKrSOHmG4kCof9rKI0anXSV6Lx7ihfa6KxUuss0UUodP6z3BdsThVNndseNvpget-9qZQ7BwvBjElH0zz1zrF9fBGGv_koXgqMlWUHnnQZGyY7RvEiGnVAKlw1aB_I2xSMuV4E-eg8D11vSNj09ebDrtlWLwXsVr7ypY0q_HGSYp6bfnq-st2OkHO4OkQCBu5Y1Kqr9iFgmFsAc1P2pdOZSnyx2NSsikRedp0AUkzySR6KvD4QOOEjI3T9to_IZXVkBc0Ue7VMPrDjVMBtA'
UNKNOWN_KID = 'eyJhbGciOiJSUzI1NiIsImtpZCI6Ims5IiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJweTJheHVtIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImV4cCI6NDEwMjQ0NDgwMH0.xUIpBBNJj63GaXv8MwAFTPPBPX5ALDQiMaSA8BueScUrxvX8EZeWXH3dH5A4r7SAahCWjEaSy7YHwngr70AJjRN-bG3nfCFacrDs5hpbZnUWHP5eWN-QDW-MGhk985CrZEhvWhralRxZMIei38dZS9C1aXa_Dtbv9Ecp1H8ZDgunB9BHMVANtQxPHppnDmf99l-e9VsVa97Q8Igva2MGqAs1MpAHf68-5n26WNFGQQ0Hxi0b_oZqRmHU_4hpRFB69avIwmupwKRoQ6j0NEqhbDubOZuiAZD6mfLrdkouFDDkbnuh1Kc_JASxmjw6TTaHp9iAPDTkn9SNH_Jvn3Is3A'
BAD_SIG = 'eyJhbGciOiJSUzI1NiIsImtpZCI6ImsxIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJweTJheHVtIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImV4cCI6NDEwMjQ0NDgwMH0.QVS3o0F2O5T_lji23LdZMzBTKvg97XODwuYWXLEvJmhgDCCzU_Tjx7yPqI_uPl_eHqbf9xqVzR6ChnEFSYEk5QPHFUWBHhLwzDHNyc9nz86aEblyGtLlbtZT8XfdwRDqczRvEZv4kY2pqobWNDujKnOP9j5FwhVKIO-JyoRvyK7-wlabyM-98LkGmwZAvz9426EJ0xuLuDAaC3nQUQcn9w35Jn_8YVboCsAj_i4-V-gqetwtsath3bwpHv3iIrSbGZfgdedxuRqg1rFINiBhQM0ZgAP92bUhr-DjT9FWw_EjbAYvv2Ryj_eSDZjLrRGATva0yXgSmQkSUHwTBhv5Mg'
EXPIRED = 'eyJhbGciOiJSUzI1NiIsImtpZCI6ImsxIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJweTJheHVtIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImV4cCI6MTAwMDAwMDAwMH0.G7teSpufblVxRgwkVB9qxXHaFBU_eXEdpOGwx9D0UWwsSc_DBd7aCAKRrD4shHgn7wRklxcw_KwyH7sz_s_hvnkFsSmKFQ3hMl5N4foAfbOzfaanfKtCQTq_6tuJzPMxcUuDlI5CKWVHj7BI-3iLknaPIWx8CTwnGZUi6QjdPGiHMeP9eNgQdLknokp_aj1Ce_nxna4Fb7e78ArfRHzprxLFY57p_KY4EC5qB4hm6rWyCEUK52oKI2bwG22iAAkSfGKR9I0_bIUxsf6omHY8PurZNYs_cLhNOoetqIVRUC5EXNeh9jqzAo1vr-zPnb2GDkYg6ksoAjk9LhXR4hLUfQ'
WRONG_AUD = 'eyJhbGciOiJSUzI1NiIsImtpZCI6ImsxIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJvdGhlciIsImlzcyI6Imh0dHBzOi8vaXNzdWVyLmV4YW1wbGUiLCJleHAiOjQxMDI0NDQ4MDB9.1qicE-AaEcgVKS8Kn55vsuoLkMr3yqjoPGUBLxNQrDKQPLGeOfmM6GS7DQP4qMidVEeYflpZ3eXJOMSpVacXLs27pkrXonpz3N3T_horwFgQlN019-7FIGAmvTZJP3LLaAukXgw0u0RDb19ywiAetOBIrpK5jyjd_czdScPeonqsxOsUyuRIYDXdXONcxwQc7_RFZDOe19Z-adOP2E99S5b7crYP7g0_kp_LmCNzEaxVMpgHPV476yAhPiifVWHv9G3f7vpo7WbiG8KR_jIHpcM28k2HwKDLVTxvwEPCappIY08ELkEXQrpq5HozQbv6S1ZiIhxWt6HJZIF2h6-Hbg'
K2 = 'eyJhbGciOiJSUzI1NiIsImtpZCI6ImsyIiwidHlwIjoiSldUIn0.eyJzdWIiOiJhbm4iLCJhdWQiOiJweTJheHVtIiwiaXNzIjoiaHR0cHM6Ly9pc3N1ZXIuZXhhbXBsZSIsImV4cCI6NDEwMjQ0NDgwMH0.LFxxbvTeLQVKhvApGFt__qMXPTWxiaz5jkF-7e-MUrD0b8OaYn0jY4vGeWv2Lun236-uhSKnWSrCeruTNOf_Cdm7Id55NnRQaupNJoB-t3rxPaCHq-6x5gAmL3aQ6YmIY4CtKGxf66izjIamdrWPK_JRAPlX5rMmJybtpTJduWGSGxXiHqVkYGtxqbZRdDs4KPv83QO5VQSBEStL64yst9779UUQDQVx6VglnVT9yi_lu4XwP0YH1rVpuA9BOckACVaUZB4uHeemrHBz7SdFppiHzW5HZmB9kI7lzFITmTYYHKnSqzxEgVKRoTHTSyQhUuznul3jHW4NTVyvON2npA'

SETS = {
    "ok": {"keys": [JWK1, JWK2]},
    "one": {"keys": [JWK1]},
    "junk": {"keys": ["not-an-object", JWK1]},
    "badnum": {"keys": [{**JWK1, "n": "!!!"}]},
    "empty": {"keys": []},
    "list": [JWK1],
}
CLIENTS = {name: PyJWKClient(f"{BASE}/jwks/keys/{name}", cache_keys=False) for name in SETS}


@router.get("/keys/{name}")
async def keys(name: str):
    if name not in SETS:
        raise HTTPException(status_code=404)
    return SETS[name]


def verify(client: PyJWKClient, token: str) -> dict:
    try:
        key = client.get_signing_key_from_jwt(token)
        claims = jwt.decode(token, key.key, algorithms=["RS256", "PS256"], audience="py2axum")
        return {"ok": True, "kid": key.key_id, "sub": claims["sub"]}
    except jwt.PyJWTError as e:
        return {"ok": False, "error": type(e).__name__, "msg": str(e)}
    except (ValueError, TypeError, AttributeError) as e:
        return {"ok": False, "error": type(e).__name__, "msg": str(e)}


TOKENS = {"valid": VALID, "ps": VALID_PS, "unknown": UNKNOWN_KID, "badsig": BAD_SIG, "expired": EXPIRED,
          "aud": WRONG_AUD, "k2": K2, "garbage": "not.a.token"}


@router.get("/verify/{keyset}/{token}")
def verify_route(keyset: str, token: str):
    return verify(CLIENTS[keyset], TOKENS[token])


@router.get("/set/{name}")
def keyset(name: str):
    try:
        s = PyJWKSet.from_dict(SETS[name])
        return {"kids": [k.key_id for k in s.keys]}
    except jwt.PyJWTError as e:
        return {"error": type(e).__name__, "msg": str(e)}
    except (ValueError, TypeError, AttributeError) as e:
        return {"error": type(e).__name__, "msg": str(e)}


# PyJWK(...) of key material each release reads its own way (base64url_decode skips characters outside the
# alphabet; cryptography's checks of the public numbers; an empty HMAC key refused from 2.14)
PYJWKS = {
    "even_n": {"kty": "RSA", "n": "Cg", "e": "Aw", "alg": "RS256"},
    "small_n": {"kty": "RSA", "n": "Ag", "e": "AQAB", "alg": "RS256"},
    "even_e": {**JWK1, "e": "BA"},
    "big_e": {**JWK1, "e": JWK1["n"]},
    "junk_n": {**JWK1, "n": "!!" + JWK1["n"][2:]},
    "hmac": {"kty": "oct", "k": "c2VjcmV0", "alg": "HS256"},
    "hmac_junk": {"kty": "oct", "k": "c2V!jcmV0", "alg": "HS256"},
    "hmac_empty": {"kty": "oct", "k": "", "alg": "HS256"},
    "hmac_int": {"kty": "oct", "k": 123, "alg": "HS256"},
}


def outcome(f):
    try:
        return {"ok": f()}
    except jwt.PyJWTError as e:
        return {"error": type(e).__name__, "msg": str(e)}
    except (ValueError, TypeError, AttributeError, OverflowError) as e:
        return {"error": type(e).__name__, "msg": str(e)}


@router.get("/pyjwk/{case}")
def pyjwk(case: str):
    def make():
        k = jwt.PyJWK(PYJWKS[case])
        signed = jwt.encode({"sub": "ann"}, "secret", algorithm="HS256")
        if case.startswith("hmac"):
            return [k.algorithm_name, jwt.decode(signed, k, algorithms=["HS256"])]
        return [k.algorithm_name, k.key_type]
    return outcome(make)


def _tok(claims: dict) -> list[str]:
    return jwt.encode(claims, "secret", algorithm="HS256").split(".")


@router.get("/decode/{case}")
def decode_case(case: str):
    """token segments each release decodes its own way, claims int() fails on, options a release changes"""
    h, p, s = _tok({"sub": "ann"})
    pad = "=" * (-len(p) % 4)
    tokens = {
        "padded": f"{h}.{p}{pad}.{s}",
        "overpadded": f"{h}.{p}{pad}====.{s}",
        "junk": f"{h[:4]}!!{h[4:]}.{p}.{s}",
        "junk_sig": f"{h}.{p}.{s[:3]}*{s[3:]}",
        "exp_list": ".".join(_tok({"sub": "ann", "exp": [1]})),
        "exp_inf": ".".join(_tok({"sub": "ann", "exp": float("inf")})),
        "nbf_dict": ".".join(_tok({"sub": "ann", "nbf": {}})),
    }
    return outcome(lambda: jwt.decode(tokens[case], "secret", algorithms=["HS256"]))


@router.get("/options")
def options_dict():
    """2.13 writes the verify_* defaults into the caller's options when verify_signature is False"""
    opts = {"verify_signature": False, "verify_aud": True}
    claims = jwt.decode(".".join(_tok({"sub": "ann", "aud": "x"})), options=opts, algorithms=["HS256"], audience="x")
    return {"claims": claims, "options": opts}
