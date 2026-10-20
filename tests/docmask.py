"""Generated documents compared byte for byte, except the fields that hold the instant or a random id.

A PDF from reportlab carries `/CreationDate (D:...)`, `/ModDate (D:...)` and the trailer's `/ID [<..><..>]`
(an md5 of the instant unless `invariant=1`); an OOXML package from openpyxl (xlsx, docx) carries the zip
entries' DOS time and date and the `dcterms:created` / `dcterms:modified` of `docProps/core.xml`. Those
values are replaced by placeholders of the same length (a length that differs stays a difference, and so
do the PDF's xref offsets); everything else is compared as bytes. A zip is walked entry by entry: its
local headers, compressed data and central directory must cover the file without a gap, otherwise the
whole file is compared raw.
"""
from __future__ import annotations

import hashlib
import re
import struct
import zlib

PDF_TYPES = ("application/pdf",)
OOXML_PREFIX = "application/vnd.openxmlformats-officedocument."

_PDF_DATE = re.compile(rb"(/(?:CreationDate|ModDate) \()(D:[^)]*)(\))")
_PDF_ID = re.compile(rb"(/ID \s*\[)(<[0-9a-fA-F]*>\s*<[0-9a-fA-F]*>)(\])")
_CORE_DATE = re.compile(rb"(<dcterms:(?:created|modified)\b[^>]*>)([^<]*)(</dcterms:)")
CORE = b"docProps/core.xml"


def _blank(m: re.Match) -> bytes:
    return m.group(1) + b"#" * len(m.group(2)) + m.group(3)


def mask_pdf(data: bytes) -> bytes:
    return _PDF_ID.sub(_blank, _PDF_DATE.sub(_blank, data))


def mask_core(xml: bytes) -> bytes:
    return _CORE_DATE.sub(_blank, xml)


def _local(data: bytes, pos: int):
    """One local entry at `pos`: (masked bytes, end), or None when it is not one this walker reads."""
    if data[pos:pos + 4] != b"PK\x03\x04" or len(data) < pos + 30:
        return None
    (flags, method, csize, nlen, xlen) = (struct.unpack_from("<H", data, pos + 6)[0], struct.unpack_from("<H", data, pos + 8)[0],
                                          struct.unpack_from("<I", data, pos + 18)[0], *struct.unpack_from("<HH", data, pos + 26))
    if flags & 0x08:  # sizes after the data: not what zipfile writes into a seekable file
        return None
    name = data[pos + 30:pos + 30 + nlen]
    start = pos + 30 + nlen + xlen
    end = start + csize
    if end > len(data):
        return None
    head = bytearray(data[pos:start])
    head[10:14] = b"\0\0\0\0"  # DOS time, date
    body = data[start:end]
    if name == CORE:
        try:
            raw = zlib.decompress(body, -15) if method == 8 else body if method == 0 else None
        except zlib.error:
            raw = None
        if raw is None:
            return None
        head[14:26] = b"\0" * 12  # crc and sizes follow the masked dates
        body = b"<core>" + mask_core(raw)
    return bytes(head) + body, end


def mask_zip(data: bytes) -> bytes | None:
    """The package with its instants masked, or None when it is not laid out the way zipfile writes it."""
    out, pos, offsets = [], 0, []
    while data[pos:pos + 4] == b"PK\x03\x04":
        entry = _local(data, pos)
        if entry is None:
            return None
        offsets.append(pos)
        out.append(entry[0])
        pos = entry[1]
    seen = 0
    while data[pos:pos + 4] == b"PK\x01\x02" and len(data) >= pos + 46:
        nlen, xlen, clen = struct.unpack_from("<HHH", data, pos + 28)
        end = pos + 46 + nlen + xlen + clen
        rec = bytearray(data[pos:end])
        if seen >= len(offsets) or struct.unpack_from("<I", rec, 42)[0] != offsets[seen]:
            return None  # the directory must point at the entries in order
        rec[12:16] = b"\0\0\0\0"
        rec[42:46] = b"\0\0\0\0"  # where the entry starts: shifts with a masked entry's compressed size
        if rec[46:46 + nlen] == CORE:
            rec[16:28] = b"\0" * 12
        out.append(bytes(rec))
        seen += 1
        pos = end
    if seen != len(offsets) or data[pos:pos + 4] != b"PK\x05\x06" or len(data) < pos + 22:
        return None
    eocd = bytearray(data[pos:])
    eocd[16:20] = b"\0\0\0\0"  # where the directory starts
    out.append(bytes(eocd))
    return b"".join(out)


def kind(ctype: str) -> str | None:
    ctype = ctype.split(";")[0].strip().lower()
    if ctype in PDF_TYPES:
        return "pdf"
    if ctype.startswith(OOXML_PREFIX):
        return "ooxml"
    return None


def masked(ctype: str, data: bytes) -> bytes:
    k = kind(ctype)
    if k == "pdf":
        return mask_pdf(data)
    if k == "ooxml":
        z = mask_zip(data)
        return data if z is None else z
    return data


def digest(ctype: str, data: bytes) -> dict:
    """What a conformance step compares of a document body."""
    # the size of what is compared: a zip's own size moves with the deflated length of core.xml's dates
    m = masked(ctype, data)
    return {"document": kind(ctype), "size": len(m), "sha256": hashlib.sha256(m).hexdigest()}
