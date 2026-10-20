"""Scenario of fixtures/pgpyapp: pgpy 0.6.0's PGPKey.from_blob() (dynrt/pgpy.rs, pgpycore.rs). No database.

Every entry of pgpy_cases.json (144 inputs run on the real pgpy when the port was written: keys made by pgpy and by
GnuPG 2.5.24, test identities only, then armor, base64, packet, signature, user id and binary-path edge cases) goes
through the application's upload route, the detail route and the static-import route; then 200 mutations of the
valid keys, from a fixed seed (base64 characters replaced, armor truncated, packet bytes changed, inserted, removed
or spliced and re-armored with a correct CRC, so that the packet parser is reached). Compare the logs too (--logs):
an inner newline in a user id is a 500 with its traceback; pgpy's warnings (CRC, orphaned packet) are dropped by the
log comparison on both sides."""
import base64
import json
import random
import re
from pathlib import Path

CASES = json.loads((Path(__file__).parent / "pgpy_cases.json").read_text())


def _crc24(data: bytes) -> int:
    crc = 0xB704CE
    for b in data:
        crc ^= b << 16
        for _ in range(8):
            crc <<= 1
            if crc & 0x1000000:
                crc ^= 0x1864CFB
    return crc & 0xFFFFFF


_ARMOR = re.compile(r"-----BEGIN PGP [A-Z ]+-----\n(?:[^\n]*: [^\n]*\n)*\n?((?:[A-Za-z0-9+/=]+\n)+)=([A-Za-z0-9+/]{4})\n")
_B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"


def _mutate(rnd: random.Random, t: str, valid: list[str]) -> str:
    k = rnd.randrange(9)
    m = _ARMOR.search(t)
    if not m:
        return t[:rnd.randrange(len(t) + 1)]
    body = m.group(1)
    raw = bytearray(base64.b64decode(body))
    if k == 0:  # base64 characters replaced (the CRC no longer matches: a warning)
        b = list(body)
        for _ in range(rnd.randint(1, 3)):
            i = rnd.randrange(len(b))
            if b[i] != "\n":
                b[i] = rnd.choice(_B64 + "=")
        return t[:m.start(1)] + "".join(b) + t[m.end(1):]
    if k == 1:  # the armored text truncated
        return t[:rnd.randrange(len(t) + 1)]
    if k == 2:
        for _ in range(rnd.randint(1, 4)):
            raw[rnd.randrange(len(raw))] = rnd.randrange(256)
    elif k == 3:
        raw[rnd.randrange(len(raw))] ^= 1 << rnd.randrange(8)
    elif k == 4:
        raw = raw[:rnd.randrange(len(raw) + 1)]
    elif k == 5:
        i = rnd.randrange(len(raw) + 1)
        raw[i:i] = bytes(rnd.randrange(256) for _ in range(rnd.randint(1, 6)))
    elif k == 6:
        i = rnd.randrange(len(raw))
        del raw[i:min(len(raw), i + rnd.randint(1, 20))]
    elif k == 7:  # a packet header byte (or the first one) replaced by another tag
        i = rnd.choice([0] + [x for x in range(len(raw)) if raw[x] & 0x80][:50])
        raw[i] = rnd.choice([0xC0 | rnd.randrange(64), 0x80 | rnd.randrange(64)])
    else:  # two keys spliced
        m2 = _ARMOR.search(rnd.choice(valid))
        if m2:
            r2 = base64.b64decode(m2.group(1))
            a = rnd.randrange(len(raw) + 1)
            raw = raw[:a] + r2[rnd.randrange(len(r2) + 1):]
    enc = base64.b64encode(bytes(raw)).decode()
    enc = "\n".join(enc[i:i + 64] for i in range(0, len(enc), 64))
    crc = base64.b64encode(_crc24(bytes(raw)).to_bytes(3, "big")).decode()
    return t[:m.start(1)] + (enc + "\n" if enc else "") + "=" + crc + "\n" + t[m.end():]


def _decodes(t: str) -> bool:
    m = _ARMOR.search(t)
    try:
        return m is not None and len(base64.b64decode(m.group(1))) > 0
    except ValueError:
        return False


def _fuzz(n: int, seed: int) -> list[str]:
    valid = [c["input"] for c in CASES if c["input"].startswith("-----BEGIN PGP") and _decodes(c["input"])]
    rnd = random.Random(seed)
    return [_mutate(rnd, rnd.choice(valid), valid) for _ in range(n)]


STEPS: list = [("GET", "/health", None)]
for c in CASES:
    for path in ("/pgp-keys", "/pgp-keys/detail", "/static/pgp-keys"):
        STEPS.append(("POST", path, {"key": c["input"]}))
for text in _fuzz(200, 20261019):
    for path in ("/pgp-keys", "/pgp-keys/detail"):
        STEPS.append(("POST", path, {"key": text}))
STEPS += [("POST", "/pgp-keys", {}), ("POST", "/pgp-keys", {"key": 12}), ("POST", "/pgp-keys/detail", {"key": None})]


def reset(db: str) -> None:
    pass
