"""Scenario of fixtures/stripeapp: stripe-python 14 against tests/stripe_stub.py (scripts_start_stripeapp.sh), a fake
key and no real account (ADR-014). The stub echoes each request (method, path, form body, headers), so the compared
responses show the request stripe-python sent as well as the objects and errors it built. Compare the logs too
(--logs): the `stripe` logger is at INFO."""
import hashlib
import hmac
import json
import time

SECRET = "whsec_fixture"


def _sig(payload: bytes, t: int, secret: str = SECRET) -> str:
    return f"t={t},v1=" + hmac.new(secret.encode(), f"{t}.".encode() + payload, hashlib.sha256).hexdigest()


EVENT = json.dumps({"id": "evt_1", "object": "event", "type": "invoice.paid", "api_version": "2026-02-25.clover",
                    "data": {"object": {"id": "in_1", "object": "invoice", "metadata": {"org": "7"}, "lines": {
                        "object": "list", "data": [{"id": "il_1", "object": "line_item", "amount": 900}], "has_more": False,
                        "url": "/v1/invoices/in_1/lines"}}}, "livemode": False}).encode()
NOW = int(time.time())

STEPS: list = [("GET", "/health", None), ("GET", "/conf", None)]
# the calls of the applications
STEPS += [("POST", "/customers", {"email": "ann@example.com", "name": "Ann", "metadata": {"source": "app-v2"}}),
          ("POST", "/customers", {"email": "e@example.com", "name": None, "metadata": {}}),
          ("GET", "/customers", None), ("GET", "/customers?email=nobody@example.com&limit=3", None),
          ("GET", "/customers/cus_123", None), ("GET", "/customers/cus_123?expand=true", None),
          ("GET", "/customers/cus_missing", None),
          ("POST", "/customers/cus_123", {"invoice_settings": {"default_payment_method": "pm_1"}}),
          ("POST", "/customers/cus_123/credit?amount=700", None),
          ("GET", "/subscriptions/sub_1", None), ("GET", "/subscriptions/sub_missing", None),
          ("POST", "/subscriptions/sub_1/cancel", None), ("POST", "/subscriptions/sub_1/cancel?at_period_end=false", None),
          ("POST", "/subscriptions/sub_1/cancel?at_period_end=false&how=delete", None),
          ("POST", "/subscriptions", {"customer": "cus_1", "price": "price_1"}),
          ("POST", "/subscriptions", {"customer": "cus_1", "price": "price_1", "trial_period_days": 14, "payment_method": "pm_9",
                                      "metadata": {"org": 3}}),
          ("POST", "/checkout", {"customer": "cus_1", "price": "price_1"}),
          ("POST", "/checkout", {"customer": "cus_1", "price": "price_1", "trial_period_days": 30, "org": 5}),
          ("GET", "/checkout/cs_test_1", None),
          ("POST", "/portal", None), ("POST", "/portal?customer=cus_missing", None),
          ("POST", "/intents", None), ("POST", "/intents?amount=99.99&email=t@example.com&key=k-1", None),
          ("GET", "/intents/pi_1", None), ("GET", "/intents/pi_missing", None),
          ("POST", "/intents/pi_1/confirm", None), ("POST", "/intents/pi_1/cancel", None),
          ("POST", "/setup", None), ("GET", "/setup/seti_1", None),
          ("POST", "/charge", None), ("POST", "/charge?pm=pm_declined&key=rent-2026-10", None),
          ("GET", "/payment-methods", None), ("POST", "/payment-methods/pm_1/detach", None),
          ("POST", "/refunds", None), ("POST", "/refunds?amount=12.5&reason=requested_by_customer", None),
          ("GET", "/invoices", None), ("GET", "/invoices?limit=3", None), ("GET", "/invoices?backwards=true", None),
          ("GET", "/invoices/pages", None),
          ("GET", "/invoices/in_1", None), ("GET", "/invoices/in_missing", None),
          ("POST", "/catalog", None), ("POST", "/promo", None),
          ("GET", "/account", None), ("GET", "/account?key=sk_test_unauth", None)]
# request options, encoding, module configuration, arguments
STEPS += [("POST", f"/options?which={w}", None) for w in ("api_key", "account", "version", "headers", "idem", "retries",
                                                          "retries0", "context")]
STEPS += [("POST", f"/encode?which={w}", None) for w in ("nested", "lists", "dates", "scalars", "keys")]
STEPS += [("POST", "/conf/version?version=2025-03-31.basil", None), ("GET", "/conf", None),
          ("POST", "/conf/base?base=http://127.0.0.1:9", None), ("POST", "/conf/base?base=", None),
          ("POST", "/conf/base?base=ftp://example.com", None), ("POST", "/conf/nokey", None)]
STEPS += [("GET", f"/args/{w}", None) for w in ("missing", "extra", "int_id", "none_id", "empty_id", "quoted", "modify_int",
                                                 "account_none", "account_id", "search", "kw_id", "kw_both", "kw_modify")]
# objects: a dict subclass
STEPS += [("GET", f"/objects/{w}", None) for w in ("str", "repr", "dict", "setattr", "setempty", "missing", "keyerror",
                                                    "list_index", "list_iter", "private", "isinstance", "fstring")]
# errors: each status, retries, bodies that are not JSON
STEPS += [("POST", f"/errors/{t}", None) for t in ("declined", "missing", "ratelimit", "noretry", "boom", "badjson", "latin1",
                                                    "noerror", "oauth", "unauth", "perm", "idem")]
STEPS += [("POST", f"/errors-http/{t}", None) for t in ("ok", "declined", "unauth")]
# stripe.error.X: an AttributeError when evaluated under stripe 14.0.0, an alias from 14.0.1
STEPS += [("POST", f"/errors-legacy{g}/{c}", None) for g in ("", "-guarded") for c in ("ok", "raise", "tuple", "value")]
# webhooks
STEPS += [("POST", "/webhook", EVENT, {"stripe-signature": _sig(EVENT, NOW)}),
          ("POST", "/webhook", EVENT, {"stripe-signature": _sig(EVENT, NOW, "whsec_other")}),
          ("POST", "/webhook", EVENT, {"stripe-signature": "garbage"}),
          ("POST", "/webhook", EVENT),
          ("POST", "/webhook", b"{not json", {"stripe-signature": _sig(b"{not json", NOW)}),
          ("POST", "/webhook", b"[1, 2]", {"stripe-signature": _sig(b"[1, 2]", NOW)}),
          ("POST", "/webhook", b"\xff\xfe", {"stripe-signature": "t=1,v1=00"})]
STEPS += [("POST", f"/webhook/signed/{c}", EVENT) for c in ("ok", "old", "old_no_tolerance", "two_sigs", "wrong_secret", "no_v1",
                                                            "no_t", "garbage", "bad_t", "spaces_t")]


def reset(db: str) -> None:
    pass

# the retry delay has a random jitter (HTTPClient._add_jitter_time), on both sides
LOG_MASKS = [(r"after sleeping \d+\.\d\d seconds", "after sleeping <s> seconds")]
