"""tests/docmask.py masks exactly a document's instants and random id: two generations a few seconds
apart compare equal, any other byte that changes is a difference."""
import io
import struct
import sys
import zipfile
import zlib
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import docmask  # noqa: E402

PDF = "application/pdf"
XLSX = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"

# what reportlab writes (4.4.5, not invariant): two instants, two ids
PDF_A = (b"%PDF-1.4\n1 0 obj\n<<\n/Author (\\(anonymous\\)) /CreationDate (D:20261018135651+02'00') "
         b"/ModDate (D:20261018135651+02'00') /Producer (ReportLab PDF Library - www.reportlab.com)\n>>\nendobj\n"
         b"2 0 obj\n<< /Length 10 >>\nstream\nBT (Bonjour) ET\nendstream\nendobj\n"
         b"trailer\n<<\n/ID \n[<e580dccabe8148c705c0de60b08534d4><e580dccabe8148c705c0de60b08534d4>]\n"
         b"% ReportLab generated PDF document -- digest (http://www.reportlab.com)\n>>\nstartxref\n1049\n%%EOF\n")
PDF_B = (PDF_A.replace(b"20261018135651+02'00'", b"20261018140212+02'00'")
         .replace(b"e580dccabe8148c705c0de60b08534d4", b"0f1e2d3c4b5a69788796a5b4c3d2e1f0"))


def flip(data: bytes, at: int) -> bytes:
    return data[:at] + bytes([data[at] ^ 1]) + data[at + 1:]


def sha(ctype, data):
    return docmask.digest(ctype, data)["sha256"]


def test_pdf_instants_and_id_masked():
    assert PDF_A != PDF_B
    assert sha(PDF, PDF_A) == sha(PDF, PDF_B)


@pytest.mark.parametrize("needle", [b"Bonjour", b"Producer", b"startxref\n1049", b"/Author", b"%PDF-1.4"])
def test_pdf_other_byte_detected(needle):
    at = PDF_A.index(needle) + len(needle) - 1
    assert sha(PDF, flip(PDF_A, at)) != sha(PDF, PDF_A)


def test_pdf_date_length_detected():
    # an instant written in another form (another length) shifts the xref offsets: a difference
    assert sha(PDF, PDF_A.replace(b"+02'00'", b"Z", 1)) != sha(PDF, PDF_A)


def core(instant: str) -> bytes:
    return ('<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n<cp:coreProperties '
            'xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties">'
            '<dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/">openpyxl</dc:creator>'
            f'<dcterms:created xmlns:dcterms="http://purl.org/dc/terms/" xsi:type="dcterms:W3CDTF">{instant}</dcterms:created>'
            f'<dcterms:modified xmlns:dcterms="http://purl.org/dc/terms/" xsi:type="dcterms:W3CDTF">{instant}</dcterms:modified>'
            '</cp:coreProperties>').encode()


def package(instant: str, when: tuple, sheet: bytes = b"<worksheet>A1</worksheet>") -> bytes:
    """What openpyxl's save does: zipfile, ZIP_DEFLATED, docProps/core.xml between other parts."""
    b = io.BytesIO()
    with zipfile.ZipFile(b, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in (("docProps/app.xml", b"<Properties/>"), ("docProps/core.xml", core(instant)),
                           ("xl/worksheets/sheet1.xml", sheet)):
            info = zipfile.ZipInfo(name, when)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)
    return b.getvalue()


X_A = package("2026-10-18T11:56:52Z", (2026, 10, 18, 13, 56, 52))
X_B = package("2026-10-18T12:07:31Z", (2026, 10, 18, 14, 7, 30))


def test_ooxml_instants_masked():
    assert X_A != X_B
    assert sha(XLSX, X_A) == sha(XLSX, X_B)


def test_ooxml_content_detected():
    assert sha(XLSX, package("2026-10-18T11:56:52Z", (2026, 10, 18, 13, 56, 52), b"<worksheet>A2</worksheet>")) != sha(XLSX, X_A)
    assert sha(XLSX, package("2026-10-18T11:56:52Z", (2026, 10, 18, 13, 56, 52))) == sha(XLSX, X_A)


def test_ooxml_core_other_field_detected():
    b = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(X_A)) as src, zipfile.ZipFile(b, "w", zipfile.ZIP_DEFLATED) as z:
        for info in src.infolist():
            data = src.read(info)
            if info.filename == "docProps/core.xml":
                data = data.replace(b"openpyxl", b"openpyxm")
            z.writestr(info, data)
    other = b.getvalue()
    assert sha(XLSX, other) != sha(XLSX, X_A)


def every_offset_but_masks(data: bytes):
    """Each byte offset of the package outside the masked fields (DOS time/date, core.xml's crc,
    sizes and compressed bytes, the offsets of the directory)."""
    masked = set()
    pos = 0
    entries = []
    while data[pos:pos + 4] == b"PK\x03\x04":
        csize, nlen, xlen = struct.unpack_from("<I", data, pos + 18)[0], *struct.unpack_from("<HH", data, pos + 26)
        name = data[pos + 30:pos + 30 + nlen]
        masked.update(range(pos + 10, pos + 14))
        start = pos + 30 + nlen + xlen
        if name == docmask.CORE:
            masked.update(range(pos + 14, pos + 26))
            masked.update(range(start, start + csize))  # compared decompressed: test_ooxml_core_other_field_detected
        entries.append(pos)
        pos = start + csize
    while data[pos:pos + 4] == b"PK\x01\x02":
        nlen, xlen, clen = struct.unpack_from("<HHH", data, pos + 28)
        masked.update(range(pos + 12, pos + 16))
        masked.update(range(pos + 42, pos + 46))
        if data[pos + 46:pos + 46 + nlen] == docmask.CORE:
            masked.update(range(pos + 16, pos + 28))
        pos += 46 + nlen + xlen + clen
    masked.update(range(pos + 16, pos + 20))
    return [i for i in range(len(data)) if i not in masked]


def test_ooxml_every_other_byte_detected():
    base = sha(XLSX, X_A)
    for at in every_offset_but_masks(X_A):
        assert sha(XLSX, flip(X_A, at)) != base, at


def test_every_pdf_byte_outside_masks_detected():
    base = sha(PDF, PDF_A)
    keep = set()
    for m in list(docmask._PDF_DATE.finditer(PDF_A)) + list(docmask._PDF_ID.finditer(PDF_A)):
        keep.update(range(m.start(2), m.end(2)))
    for at in range(len(PDF_A)):
        if at in keep:
            continue
        # a flip that keeps the field's syntax but moves its bounds is still a difference
        assert sha(PDF, flip(PDF_A, at)) != base, at


def test_real_generators_if_installed():
    reportlab = pytest.importorskip("reportlab")
    openpyxl = pytest.importorskip("openpyxl")
    import time

    from reportlab.lib.styles import getSampleStyleSheet
    from reportlab.platypus import Paragraph, SimpleDocTemplate

    def pdf():
        b = io.BytesIO()
        SimpleDocTemplate(b).build([Paragraph("Bonjour", getSampleStyleSheet()["Normal"])])
        return b.getvalue()

    def xlsx():
        wb = openpyxl.Workbook()
        wb.active["A1"] = "x"
        b = io.BytesIO()
        wb.save(b)
        return b.getvalue()

    p1, x1 = pdf(), xlsx()
    time.sleep(2.1)  # past a DOS time's 2 s and a PDF date's 1 s
    p2, x2 = pdf(), xlsx()
    assert p1 != p2 and x1 != x2
    assert sha(PDF, p1) == sha(PDF, p2) and sha(XLSX, x1) == sha(XLSX, x2)
    assert reportlab and zlib


def test_not_a_document_left_alone():
    assert docmask.kind("application/json") is None
    assert docmask.kind("application/pdf; charset=binary") == "pdf"
    # a package not laid out like zipfile's (garbage): compared raw
    assert docmask.masked(XLSX, b"PK\x03\x04garbage") == b"PK\x03\x04garbage"


def test_ooxml_deflated_size_of_the_dates_not_compared():
    # core.xml's dates deflate to lengths that differ by a byte or two: the package's size moves with them
    sizes = {}
    for sec in range(60):
        data = package(f"2026-10-18T11:56:{sec:02d}Z", (2026, 10, 18, 13, 56, sec - sec % 2))
        sizes.setdefault(len(data), data)
    assert len({docmask.digest(XLSX, d)["sha256"] for d in sizes.values()}) == 1
    assert len({docmask.digest(XLSX, d)["size"] for d in sizes.values()}) == 1
