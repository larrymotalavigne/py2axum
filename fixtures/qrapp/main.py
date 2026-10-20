"""qrcode 8.2 with Pillow's PilImage (dynrt/qr.rs): QRCode, add_data, make, make_image, qrcode.make, image.save into an
io.BytesIO as PNG, compared byte for byte (the data URI of a TOTP setup, as a real application builds it), and the
exceptions qrcode raises. No database."""
import base64
import io

import qrcode
import qrcode.constants
from fastapi import FastAPI
from qrcode.exceptions import DataOverflowError

app = FastAPI()

LEVELS = {"L": qrcode.constants.ERROR_CORRECT_L, "M": qrcode.constants.ERROR_CORRECT_M,
          "Q": qrcode.ERROR_CORRECT_Q, "H": qrcode.ERROR_CORRECT_H}


def failure(e: Exception) -> dict:
    return {"type": type(e).__name__, "str": str(e)}


def png_uri(img, **kw) -> str:
    buffer = io.BytesIO()
    img.save(buffer, **kw)
    buffer.seek(0)
    return "data:image/png;base64," + base64.b64encode(buffer.getvalue()).decode()


@app.get("/health")
async def health():
    return {"ok": True}


@app.get("/qr/totp")
async def qr_totp(email: str = "alice@example.com", secret: str = "JBSWY3DPEHPK3PXP", issuer: str = "Acme"):
    # the shape of a TOTP setup endpoint: version=1 grown by fit, PNG as a data URL
    uri = f"otpauth://totp/{issuer}:{email.replace('@', '%40')}?secret={secret}&issuer={issuer}"
    qr = qrcode.QRCode(version=1, box_size=10, border=4)
    qr.add_data(uri)
    qr.make(fit=True)
    img = qr.make_image(fill_color="black", back_color="white")
    buffer = io.BytesIO()
    img.save(buffer, format="PNG")
    buffer.seek(0)
    img_base64 = base64.b64encode(buffer.getvalue()).decode()
    return {"qr": f"data:image/png;base64,{img_base64}", "version": qr.version, "count": qr.modules_count}


@app.get("/qr/data")
async def qr_data(d: str = "hello", v: int | None = None, ec: str = "M", box: int = 10, border: int = 4,
                  mask: int | None = None, fit: bool = True, opt: int = 20, rep: int = 1, colors: str = "black,white"):
    try:
        qr = qrcode.QRCode(version=v, error_correction=LEVELS[ec], box_size=box, border=border, mask_pattern=mask)
        qr.add_data(d * rep, optimize=opt)
        qr.make(fit=fit)
        fill, back = colors.split(",")
        img = qr.make_image(fill_color=fill, back_color=back)
        return {"png": png_uri(img, format="PNG"), "version": qr.version, "mask": qr.mask_pattern,
                "size": img.pixel_size, "width": img.width}
    except (ValueError, TypeError, IndexError, DataOverflowError) as e:
        return failure(e)


@app.get("/qr/bytes")
async def qr_bytes(n: int = 10, byte: int = 0, v: int | None = None, ec: str = "M", fit: bool = True):
    try:
        qr = qrcode.QRCode(version=v, error_correction=LEVELS[ec])
        qr.add_data(bytes([byte]) * n)
        qr.make(fit=fit)
        return {"png": png_uri(qr.make_image()), "version": qr.version}
    except Exception as e:
        return failure(e)


@app.get("/qr/int/{n}")
async def qr_int(n: int):
    qr = qrcode.QRCode(box_size=2, border=0)
    qr.add_data(n, optimize=0)
    return {"png": png_uri(qr.make_image(), format="png"), "version": qr.version}


@app.get("/qr/make")
async def qr_make(d: str = "https://example.com/", box: int = 3):
    img = qrcode.make(d, box_size=box, error_correction=qrcode.ERROR_CORRECT_H)
    return {"png": png_uri(img, kind="PNG"), "size": list(img.size), "mode": img.mode}


@app.get("/qr/twice")
async def qr_twice():
    # a second add_data after make: the data cache is dropped, the version kept as the start of the next fit
    qr = qrcode.QRCode()
    qr.add_data("first")
    qr.make()
    first = qr.version
    qr.add_data("x" * 200)
    img = qr.make_image()
    buffer = io.BytesIO(b"prefix")
    buffer.seek(3)
    img.save(buffer)
    data = buffer.getvalue()
    qr.clear()
    qr.add_data("123")
    return {"first": first, "second": qr.version, "head": data[:12].hex(), "len": len(data),
            "after": png_uri(qr.make_image())}


@app.get("/qr/init")
async def qr_init(case: str):
    try:
        if case == "box0":
            qrcode.QRCode(box_size=0)
        elif case == "boxstr":
            qrcode.QRCode(box_size="7")
        elif case == "boxbad":
            qrcode.QRCode(box_size="x")
        elif case == "border":
            qrcode.QRCode(border=-1)
        elif case == "v0":
            qrcode.QRCode(version=0)
        elif case == "v41":
            qrcode.QRCode(version=41)
        elif case == "mask8":
            qrcode.QRCode(mask_pattern=8)
        elif case == "maskstr":
            qrcode.QRCode(mask_pattern="1")
        elif case == "ec4":
            qr = qrcode.QRCode(error_correction=4)
            qr.add_data("x")
            qr.make()
        elif case == "ec4fixed":
            qr = qrcode.QRCode(version=2, error_correction=4)
            qr.add_data("x")
            qr.make(fit=False)
        elif case == "positional":
            qr = qrcode.QRCode(3, qrcode.ERROR_CORRECT_L, 1, 0)
            qr.add_data("positional")
            return {"png": png_uri(qr.make_image())}
        elif case == "embedded":
            qrcode.QRCode().make_image(embeded_image_path="x.png")
        return {"ok": case}
    except Exception as e:
        return failure(e)
