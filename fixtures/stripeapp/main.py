"""stripe-python 14 (dynrt/stripe.rs): the calls of two production applications, and the usual surface around them.
Against tests/stripe_stub.py (STRIPE_API_BASE), with a fake key: never the real API (ADR-014). Each route returns
what stripe-python built (objects, errors, their attributes), the stub's `echo` showing the request it sent."""
import asyncio
import datetime
import hashlib
import hmac
import logging
import os
import time
from decimal import Decimal
from typing import Any

import stripe
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse

logging.basicConfig(level=logging.WARNING, format="%(levelname)s %(name)s %(message)s")
logging.getLogger("stripe").setLevel(logging.INFO)

stripe.api_key = os.environ.get("STRIPE_API_KEY", "sk_test_fixture")
stripe.api_base = os.environ.get("STRIPE_API_BASE", "http://127.0.0.1:9")
WEBHOOK_SECRET = "whsec_fixture"

app = FastAPI()


@app.get("/health")
async def health():
    return {"ok": True}


def describe(e: Exception) -> dict[str, Any]:
    """What an `except stripe.StripeError as e` sees."""
    out: dict[str, Any] = {"type": type(e).__name__, "str": str(e), "repr": repr(e), "args": list(e.args)}
    if isinstance(e, stripe.StripeError):
        # json_body: the response's JSON, or (an APIError for a body that is not JSON) its headers
        headers_as_body = hasattr(e.json_body, "keys") and any(k.lower() == "content-type" for k in e.json_body)
        body = sorted(k.lower() for k in e.json_body) if headers_as_body else e.json_body
        out.update({"user_message": e.user_message, "http_status": e.http_status, "http_body": e.http_body,
                    "json_body": body, "code": e.code, "request_id": e.request_id,
                    "error": dict(e.error) if e.error is not None else None,
                    "is_card": isinstance(e, stripe.CardError),
                    "is_invalid": isinstance(e, stripe.InvalidRequestError),
                    "is_param_code": isinstance(e, stripe.StripeErrorWithParamCode)})
        if isinstance(e, (stripe.CardError, stripe.InvalidRequestError)):
            out["param"] = e.param
        if isinstance(e, stripe.APIConnectionError):
            out["should_retry"] = e.should_retry
        if isinstance(e, stripe.SignatureVerificationError):
            out["sig_header"] = e.sig_header
    return out


async def guarded(call):
    try:
        return {"ok": await asyncio.to_thread(call)}
    except stripe.StripeError as e:
        return {"error": describe(e)}
    except (TypeError, ValueError, AttributeError, KeyError) as e:
        return {"error": {"type": type(e).__name__, "str": str(e)}}


# ---------------------------------------------------------------- the resource calls of the applications


@app.post("/customers")
async def customer_create(body: dict[str, Any]):
    return await guarded(lambda: stripe.Customer.create(**body))


@app.get("/customers")
async def customer_list(email: str = "a@example.com", limit: int = 1):
    customers = await asyncio.to_thread(stripe.Customer.list, email=email, limit=limit)
    first = customers.data[0].id if customers.data else None
    return {"first": first, "len": len(customers), "truth": bool(customers), "list": customers,
            "ids": [c.id for c in customers], "has_more": customers.has_more, "url": customers.url}


@app.get("/customers/{cid}")
async def customer_retrieve(cid: str, expand: bool = False):
    if expand:
        return await guarded(lambda: stripe.Customer.retrieve(cid, expand=["default_source", "invoice_settings.default_payment_method"]))
    return await guarded(lambda: stripe.Customer.retrieve(cid))


@app.post("/customers/{cid}")
async def customer_modify(cid: str, body: dict[str, Any]):
    return await guarded(lambda: stripe.Customer.modify(cid, **body))


@app.post("/customers/{cid}/credit")
async def customer_credit(cid: str, amount: int = 500):
    return await guarded(lambda: stripe.Customer.create_balance_transaction(cid, amount=-abs(amount), currency="eur",
                         description="Referral credit"))


@app.get("/subscriptions/{sid}")
async def subscription_fetch(sid: str):
    """A production fetch_subscription: `subscription.items` is dict.items, and current_period_* moved to the items."""
    try:
        sub = await asyncio.to_thread(stripe.Subscription.retrieve, sid, expand=["latest_invoice", "customer", "items.data.price"])
    except stripe.StripeError as e:
        return {"error": describe(e)}
    out: dict[str, Any] = {"id": sub.id, "status": sub.status, "customer": sub.customer, "metadata": sub.metadata,
                           "get_start": sub.get("current_period_start"), "items_get": sub.get("items")}
    for what in ("current_period_start", "items.data"):
        try:
            out[what] = sub.current_period_start if what == "current_period_start" else sub.items.data
        except AttributeError as e:
            out[what] = f"AttributeError: {e}"
    item = sub["items"]["data"][0]
    out["price"] = {"id": item.price.id, "amount": item.price.unit_amount, "interval": item.price.recurring.interval}
    out["latest_invoice"] = {"id": sub.latest_invoice.id, "status": sub.latest_invoice.status}
    return out


@app.post("/subscriptions/{sid}/cancel")
async def subscription_cancel(sid: str, at_period_end: bool = True, how: str = "modify"):
    if at_period_end:
        return await guarded(lambda: stripe.Subscription.modify(sid, cancel_at_period_end=True))
    if how == "delete":
        return await guarded(lambda: stripe.Subscription.delete(sid))
    return await guarded(lambda: stripe.Subscription.cancel(sid, invoice_now=True, prorate=False))


@app.post("/subscriptions")
async def subscription_create(body: dict[str, Any]):
    params: dict[str, Any] = {"customer": body["customer"], "items": [{"price": body["price"]}], "metadata": body.get("metadata") or {}}
    if body.get("trial_period_days"):
        params["trial_period_days"] = body["trial_period_days"]
    if body.get("payment_method"):
        await asyncio.to_thread(stripe.PaymentMethod.attach, body["payment_method"], customer=body["customer"])
        await asyncio.to_thread(stripe.Customer.modify, body["customer"], invoice_settings={"default_payment_method": body["payment_method"]})
        params["default_payment_method"] = body["payment_method"]
    return await guarded(lambda: stripe.Subscription.create(**params))


@app.post("/checkout")
async def checkout_create(body: dict[str, Any]):
    trial = body.get("trial_period_days")
    session = await asyncio.to_thread(
        stripe.checkout.Session.create,
        customer=body.get("customer"),
        payment_method_types=["card"],
        line_items=[{"price": body["price"], "quantity": 1}],
        mode="subscription",
        success_url="https://app.example/ok?session_id={CHECKOUT_SESSION_ID}",
        cancel_url="https://app.example/ko",
        metadata={"source": "fixture", "org": body.get("org", 1)},
        allow_promotion_codes=True,
        billing_address_collection="auto",
        customer_update={"address": "auto", "name": "auto"},
        subscription_data={"trial_period_days": trial, "trial_settings": {"end_behavior": {"missing_payment_method": "cancel"}}} if trial else {},
    )
    return {"url": session.url, "session_id": session.id, "echo": session["echo"]}


@app.get("/checkout/{sid}")
async def checkout_retrieve(sid: str):
    s = await asyncio.to_thread(stripe.checkout.Session.retrieve, sid)
    return {"id": s.id, "payment_status": s.payment_status, "status": s.status, "customer": s.customer,
            "metadata": dict(s.metadata) if s.metadata else {}, "echo": s["echo"]}


@app.post("/portal")
async def portal(customer: str = "cus_x"):
    return await guarded(lambda: stripe.billing_portal.Session.create(customer=customer, return_url="https://app.example/billing"))


@app.post("/intents")
async def intent_create(amount: float = 12.34, email: str | None = None, key: str | None = None):
    return await guarded(lambda: stripe.PaymentIntent.create(amount=int(amount * 100), currency="eur", receipt_email=email,
                         description="Loyer", metadata={"lease": 7, "auto": True, "none": None},
                         automatic_payment_methods={"enabled": True}, idempotency_key=key))


@app.get("/intents/{pid}")
async def intent_retrieve(pid: str):
    try:
        intent = await asyncio.to_thread(stripe.PaymentIntent.retrieve, pid)
        return {"id": intent.id, "amount": intent.amount / 100, "status": intent.status, "metadata": intent.metadata}
    except stripe.StripeError as e:
        return JSONResponse({"detail": f"introuvable: {e}"}, status_code=404)


@app.post("/intents/{pid}/{action}")
async def intent_action(pid: str, action: str):
    fn = {"confirm": stripe.PaymentIntent.confirm, "cancel": stripe.PaymentIntent.cancel}[action]
    return await guarded(lambda: fn(pid))


@app.post("/setup")
async def setup_create(customer: str = "cus_x"):
    return await guarded(lambda: stripe.SetupIntent.create(customer=customer, payment_method_types=["sepa_debit"], usage="off_session",
                         metadata={}))


@app.get("/setup/{sid}")
async def setup_retrieve(sid: str):
    return await guarded(lambda: stripe.SetupIntent.retrieve(sid))


@app.post("/charge")
async def charge(customer: str = "cus_x", pm: str = "pm_1", key: str | None = None):
    return await guarded(lambda: stripe.PaymentIntent.create(amount=int(1500.5 * 100), currency="eur", customer=customer,
                         payment_method=pm, payment_method_types=["sepa_debit"], off_session=True, confirm=True,
                         description="Prélèvement de loyer", metadata={"montant": Decimal("1500.50")}, idempotency_key=key))


@app.get("/payment-methods")
async def payment_methods(customer: str = "cus_x"):
    pms = stripe.PaymentMethod.list(customer=customer, type="card")
    return {"ids": [p.id for p in pms.data], "count": len(pms.data)}


@app.post("/payment-methods/{pm}/detach")
async def detach(pm: str):
    return await guarded(lambda: stripe.PaymentMethod.detach(pm))


@app.post("/refunds")
async def refund(intent: str = "pi_1", amount: float | None = None, reason: str | None = None):
    params: dict[str, Any] = {"payment_intent": intent}
    if amount is not None:
        params["amount"] = int(amount * 100)
    if reason:
        params["reason"] = reason
    return await guarded(lambda: stripe.Refund.create(**params))


@app.get("/invoices")
async def invoices(customer: str = "cus_x", limit: int = 2, backwards: bool = False):
    def _fetch():
        if backwards:
            page = stripe.Invoice.list(customer=customer, limit=limit, ending_before="inv_5")
        else:
            page = stripe.Invoice.list(customer=customer, limit=limit)
        return [{"id": inv.id, "number": inv.number, "amount_paid": inv.amount_paid, "created": inv.created}
                for inv in page.auto_paging_iter()][:limit + 1]

    return {"items": await asyncio.to_thread(_fetch)}


@app.get("/invoices/pages")
async def invoice_pages():
    page = stripe.Invoice.list(limit=2, status="paid")
    nxt = page.next_page()
    prev = nxt.previous_page()
    last = nxt.next_page().next_page()
    return {"first": [i.id for i in page], "next": [i.id for i in nxt], "prev": [i.id for i in prev],
            "last": [i.id for i in last], "empty": last.is_empty, "echo_next": nxt["echo"]["path"]}


@app.get("/invoices/{iid}")
async def invoice_get(iid: str):
    try:
        inv = stripe.Invoice.retrieve(iid)
    except stripe.StripeError as e:
        return {"error": describe(e)}
    try:
        pi = inv.payment_intent
    except AttributeError as e:
        pi = f"AttributeError: {e}"
    return {"id": inv.id, "pdf": inv.invoice_pdf, "payment_intent": pi}


@app.post("/catalog")
async def catalog(name: str = "Pro", amount: float = 9.0):
    product = await asyncio.to_thread(stripe.Product.create, name=name, description=None, metadata={"tier": "pro"})
    price = await asyncio.to_thread(stripe.Price.create, product=product.id, unit_amount=int(amount * 100), currency="eur",
                                    recurring={"interval": "month"}, metadata={})
    return {"product": product.id, "price": price.id, "unit_amount": price.unit_amount / 100,
            "interval": price.recurring.interval, "echo": price["echo"]}


@app.post("/promo")
async def promo(code: str = "WELCOME", percent: int = 20):
    coupon = await asyncio.to_thread(stripe.Coupon.create, percent_off=percent, duration="repeating", duration_in_months=3,
                                     name=f"{code} -{percent}%")
    promo = await asyncio.to_thread(stripe.PromotionCode.create, promotion={"type": "coupon", "coupon": coupon.id}, code=code,
                                    max_redemptions=100, expires_at=datetime.datetime(2027, 1, 1, tzinfo=datetime.UTC))
    listed = await asyncio.to_thread(stripe.PromotionCode.list, limit=100)
    off = await asyncio.to_thread(stripe.PromotionCode.modify, promo.id, active=False)
    return {"coupon": coupon, "promo": promo.id, "listed": [p.id for p in listed], "off": off["echo"]}


@app.get("/account")
async def account(key: str | None = None):
    previous = stripe.api_key
    if key is not None:
        stripe.api_key = key
    try:
        acct = await asyncio.to_thread(stripe.Account.retrieve)
        events = await asyncio.to_thread(lambda: stripe.Event.list(limit=100))
        return {"id": acct.id, "name": acct.business_profile.name, "events": len(events.data), "echo": acct["echo"]}
    except stripe.AuthenticationError as e:
        return {"auth": describe(e)}
    finally:
        stripe.api_key = previous


# ---------------------------------------------------------------- options, encoding, configuration


@app.post("/options")
async def options(which: str):
    calls = {
        "api_key": lambda: stripe.Customer.create(email="o@example.com", api_key="sk_test_other"),
        "account": lambda: stripe.Customer.create(email="o@example.com", stripe_account="acct_123"),
        "version": lambda: stripe.Customer.retrieve("cus_v", stripe_version="2024-06-20"),
        "headers": lambda: stripe.Customer.create(email="o@example.com", headers={"X-Extra": "1", "Stripe-Version": "2020-08-27"}),
        "idem": lambda: stripe.Customer.create(email="o@example.com", idempotency_key="fixed-key-1"),
        "retries": lambda: stripe.Customer.create(email="boom@example.com", max_network_retries=1),
        "retries0": lambda: stripe.Customer.create(email="boom@example.com", max_network_retries=0),
        "context": lambda: stripe.Customer.create(email="o@example.com", stripe_context="ctx_1"),
    }
    return await guarded(calls[which])


@app.post("/encode")
async def encode(which: str):
    when = datetime.datetime(2026, 10, 18, 12, 30, 15, 999999, tzinfo=datetime.timezone(datetime.timedelta(hours=2)))
    params = {
        "nested": {"metadata": {"a": 1, "b": True, "c": None, "d": 1.5, "e": "x y&z=é[]", "f": Decimal("2.50")},
                   "address": {"line1": "1 rue", "city": None}},
        "lists": {"items": [{"price": "p1", "quantity": 2}, "plain", 3, True, None, [1, 2]], "expand": ["a", "b.c"],
                  "tuple": ("x", "y")},
        "dates": {"at": when, "day": datetime.date(2026, 10, 18), "in_list": [when]},
        "scalars": {"empty": {}, "zero": 0, "false": False, "str_int": "007", "neg": -3, "big": 2**40},
        "keys": {"metadata": {1: "int key", "x y": "space"}},
    }[which]
    return await guarded(lambda: stripe.Customer.create(**params))


@app.get("/conf")
async def conf():
    return {"key_prefix": (stripe.api_key or "")[:8], "api_version": stripe.api_version, "retries": stripe.max_network_retries,
            "telemetry": stripe.enable_telemetry, "tolerance": stripe.Webhook.DEFAULT_TOLERANCE}


@app.post("/conf/version")
async def conf_version(version: str):
    previous = stripe.api_version
    stripe.api_version = version
    try:
        return await guarded(lambda: stripe.Customer.retrieve("cus_ver"))
    finally:
        stripe.api_version = previous


@app.post("/conf/base")
async def conf_base(base: str):
    previous = stripe.api_base
    stripe.api_base = base
    try:
        return await guarded(lambda: stripe.Customer.retrieve("cus_base", max_network_retries=0))
    finally:
        stripe.api_base = previous


@app.post("/conf/nokey")
async def conf_nokey():
    previous = stripe.api_key
    stripe.api_key = None
    try:
        return await guarded(lambda: stripe.Customer.list())
    finally:
        stripe.api_key = previous


@app.get("/args/{which}")
async def args(which: str):
    calls = {
        "missing": lambda: stripe.Customer.retrieve(),
        "extra": lambda: stripe.Customer.retrieve("cus_a", "cus_b"),
        "int_id": lambda: stripe.Customer.retrieve(5),
        "none_id": lambda: stripe.Customer.retrieve(None),
        "empty_id": lambda: stripe.Customer.retrieve(""),
        "quoted": lambda: stripe.Customer.retrieve("cus a/b?é"),
        "modify_int": lambda: stripe.Customer.modify(5, name="x"),
        "account_none": lambda: stripe.Account.retrieve(None),
        "account_id": lambda: stripe.Account.retrieve("acct_1"),
        "search": lambda: stripe.Customer.search(query="email:'a@example.com'"),
        "kw_id": lambda: stripe.Customer.retrieve(id="cus_kw", expand=["x"]),
        "kw_both": lambda: stripe.Customer.retrieve("cus_a", id="cus_b"),
        "kw_modify": lambda: stripe.Customer.modify(name="n", id="cus_m"),
    }
    return await guarded(calls[which])


# ---------------------------------------------------------------- objects


@app.get("/objects/{which}")
async def objects(which: str):
    c = stripe.Customer.retrieve("cus_obj")
    lst = stripe.Customer.list(limit=3)
    out: Any
    if which == "str":
        del c["echo"]
        out = str(c)
    elif which == "repr":
        del c["echo"]
        r = repr(c)
        out = [r.split(" at ")[0], r.split("> JSON: ")[1]]
    elif which == "dict":
        out = {"dict": dict(c), "keys": list(c.keys()), "len": len(c), "in": "email" in c, "get": c.get("nope", "dflt"),
               "copy_type": type(c.copy()).__name__, "to_dict": c.to_dict(), "items": [k for k, _ in c.items()]}
    elif which == "setattr":
        c.nickname = "Bob"
        c["note"] = "n"
        out = {"nickname": c["nickname"], "note": c.note}
    elif which == "setempty":
        del c["echo"]
        try:
            c.nickname = ""
            out = "no error"
        except ValueError as e:
            out = str(e)
    elif which == "missing":
        try:
            out = c.nope
        except AttributeError as e:
            out = {"attr": str(e), "args": list(e.args), "getattr": getattr(c, "nope", "dflt"), "hasattr": hasattr(c, "email")}
    elif which == "keyerror":
        try:
            out = c["nope"]
        except KeyError as e:
            out = {"key": str(e)}
    elif which == "list_index":
        try:
            out = lst[0]
        except KeyError as e:
            out = str(e)
    elif which == "list_iter":
        out = {"iter": [x.id for x in lst], "list": [x.id for x in list(lst)], "len": len(lst), "bool": bool(lst),
               "keys": sorted(lst.keys()), "data_in": "data" in lst, "object": lst.object}
    elif which == "private":
        try:
            out = getattr(c, "_" + "secret")
        except AttributeError as e:
            out = f"AttributeError: {e}"
    elif which == "isinstance":
        out = {"dict": isinstance(c, dict), "metadata_dict": isinstance(c.metadata, dict), "list_dict": isinstance(lst, dict)}
    elif which == "fstring":
        meta = c.metadata
        out = f"{meta}|{c.id}|{bool(meta)}"
    else:
        out = None
    return {"out": out}


# ---------------------------------------------------------------- errors


@app.post("/errors/{trigger}")
async def errors(trigger: str):
    try:
        await asyncio.to_thread(stripe.Customer.create, email=f"{trigger}@example.com", max_network_retries=2)
        return {"ok": True}
    except stripe.CardError as e:
        return {"card": describe(e), "user_message": e.user_message}
    except stripe.StripeError as e:
        logging.getLogger("fixture").warning("Erreur Stripe (%s) : %s", trigger, e)
        return {"error": describe(e)}


@app.post("/errors-http/{trigger}")
async def errors_http(trigger: str):
    from fastapi import HTTPException
    try:
        intent = await asyncio.to_thread(stripe.PaymentIntent.create, amount=100, currency="eur", description=trigger)
        return {"id": intent.id}
    except stripe.StripeError as e:
        raise HTTPException(status_code=400, detail=f"Erreur de paiement. Veuillez réessayer. ({e.user_message})")


async def legacy(case: str):
    """`stripe.error.X` (a real application): absent from stripe 14.0.0, whose `__getattr__` then raises a bare AttributeError
    only when the except clause is evaluated (an exception tested against it); an alias of stripe.X from 14.0.1."""
    try:
        if case == "ok":
            intent = await asyncio.to_thread(stripe.PaymentIntent.retrieve, "pi_1")
            return {"id": intent.id}
        if case == "value":
            return {"same": stripe.error.StripeError is stripe.StripeError}
        if case == "tuple":
            raise ValueError("not stripe")
        await asyncio.to_thread(stripe.Invoice.retrieve, "in_missing")
        return {"ok": True}
    except (KeyError, stripe.error.CardError) as e:
        return {"tuple": type(e).__name__}
    except stripe.error.StripeError as e:
        return {"caught": describe(e)}


@app.post("/errors-legacy/{case}")
async def errors_legacy(case: str):
    return await legacy(case)


@app.post("/errors-legacy-guarded/{case}")
async def errors_legacy_guarded(case: str):
    """The AttributeError of 14.0.0 seen by an outer handler: its message and chain (KeyError, then the tested one)."""
    try:
        return await legacy(case)
    except AttributeError as e:
        ctx = e.__context__
        return {"type": type(e).__name__, "str": str(e), "args": list(e.args), "ctx": type(ctx).__name__,
                "ctx_args": list(ctx.args), "ctx2": type(ctx.__context__).__name__ if ctx.__context__ else None}


# ---------------------------------------------------------------- webhooks


def signature(payload: str, t: int, secret: str = WEBHOOK_SECRET) -> str:
    mac = hmac.new(secret.encode("utf-8"), msg=f"{t}.{payload}".encode("utf-8"), digestmod=hashlib.sha256).hexdigest()
    return f"t={t},v1={mac}"


@app.post("/webhook")
async def webhook(request: Request):
    payload = await request.body()
    sig = request.headers.get("stripe-signature", "")
    try:
        event = stripe.Webhook.construct_event(payload, sig, WEBHOOK_SECRET)
    except ValueError as e:
        return JSONResponse({"detail": f"Invalid payload: {e}", "type": type(e).__name__}, status_code=400)
    except stripe.SignatureVerificationError as e:
        return JSONResponse({"detail": f"Invalid signature: {e}", "err": describe(e)}, status_code=400)
    obj = event.data.object
    return {"type": event["type"], "id": event.id, "object_id": obj.id, "object_class": obj.object,
            "metadata": dict(obj.get("metadata") or {}), "event": event}


@app.post("/webhook/signed/{case}")
async def webhook_signed(case: str, request: Request):
    """The signature computed here (time-dependent), then verified."""
    payload = (await request.body()).decode()
    now = int(time.time())
    header = {
        "ok": signature(payload, now),
        "old": signature(payload, 1000),
        "old_no_tolerance": signature(payload, 1000),
        "two_sigs": signature(payload, now) + ",v1=deadbeef",
        "wrong_secret": signature(payload, now, "whsec_other"),
        "no_v1": "t=1700000000,v0=abc",
        "no_t": "v1=abc",
        "garbage": "nonsense",
        "bad_t": "t=abc,v1=abc",
        "spaces_t": signature(payload, now).replace(f"t={now}", f"t= {now} "),
    }[case]
    try:
        if case == "old_no_tolerance":
            ok = stripe.WebhookSignature.verify_header(payload, header, WEBHOOK_SECRET, None)
            return {"verified": ok}
        event = stripe.Webhook.construct_event(payload, header, WEBHOOK_SECRET)
        return {"type": event.type, "data": event.data.object.id}
    except ValueError as e:
        return {"value_error": type(e).__name__, "msg": str(e)}
    except stripe.SignatureVerificationError as e:
        d = describe(e)
        d["sig_header"] = d["sig_header"].split(",")[0] if case not in ("ok", "two_sigs", "spaces_t", "wrong_secret") else "<signed>"
        d["http_body"] = None
        return {"sve": {k: v for k, v in d.items() if k in ("type", "str", "user_message", "http_status", "sig_header")}}
