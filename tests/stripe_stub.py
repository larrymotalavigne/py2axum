"""A fake Stripe API for the stripe conformance (fixtures/stripeapp): never a real key, card or account (ADR-014).

    python tests/stripe_stub.py <port>

Every response is an API object shaped like Stripe's (an `object` field, list objects with `data`/`has_more`/`url`)
built from the request itself, plus `echo`: the request as received (method, path and query, form body, headers),
normalised where stripe-python is not deterministic (the generated Idempotency-Key, the platform fields of
X-Stripe-Client-User-Agent) and without the transport headers (Host, Content-Length, Accept-Encoding:
docs/supported.md) nor X-Stripe-Client-Telemetry (per thread in stripe-python). The fixture returns what stripe-python built, so a request or a conversion that
differs shows in the compared response. Ids are hashes of the request (the same for the reference and the binary).

Errors on demand: an id or a parameter value containing a trigger word (`missing`, `declined`, `ratelimit`, `boom`,
`noretry`, `badjson`, `noerror`, `oauth`, `unauth`, `perm`, `idem`, `latin1`) answers like Stripe does.
"""
import hashlib
import json
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, unquote_plus, urlsplit

UUID4 = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
ATTEMPTS: dict[str, int] = {}
LOCK = threading.Lock()

# collection path -> object name
OBJECTS = {"customers": "customer", "subscriptions": "subscription", "checkout/sessions": "checkout.session",
           "billing_portal/sessions": "billing_portal.session", "payment_intents": "payment_intent",
           "setup_intents": "setup_intent", "payment_methods": "payment_method", "refunds": "refund", "invoices": "invoice",
           "prices": "price", "products": "product", "coupons": "coupon", "promotion_codes": "promotion_code",
           "events": "event", "accounts": "account", "account": "account", "balance_transactions": "customer_balance_transaction"}
CANNED = {
    "customer": {"email": "a@example.com", "name": "Ann", "created": 1700000000, "metadata": {}},
    "subscription": {"status": "active", "customer": "cus_x", "cancel_at_period_end": False, "canceled_at": None,
                     "trial_end": None, "metadata": {"plan": "pro"},
                     "items": {"object": "list", "data": [{"id": "si_1", "object": "subscription_item",
                               "current_period_start": 1700000000, "current_period_end": 1702592000,
                               "price": {"id": "price_1", "object": "price", "unit_amount": 900, "currency": "eur",
                                         "recurring": {"interval": "month", "interval_count": 1}}}],
                               "has_more": False, "url": "/v1/subscription_items?subscription=sub_x"},
                     "latest_invoice": {"id": "in_9", "object": "invoice", "status": "paid", "amount_paid": 900}},
    "checkout.session": {"url": "https://checkout.example/pay", "payment_status": "unpaid", "status": "open",
                         "customer": None, "subscription": None, "metadata": {"k": "v"}},
    "billing_portal.session": {"url": "https://portal.example/s"},
    "payment_intent": {"amount": 1234, "currency": "eur", "status": "requires_payment_method", "client_secret": "pi_secret",
                       "receipt_email": None, "description": None, "metadata": {}},
    "setup_intent": {"status": "requires_payment_method", "client_secret": "seti_secret", "payment_method": None,
                     "mandate": None, "customer": None},
    "refund": {"amount": 500, "status": "succeeded", "reason": None},
    "invoice": {"number": "INV-1", "status": "paid", "amount_paid": 900, "currency": "eur", "created": 1700000000,
                "invoice_pdf": "https://files.example/in.pdf", "hosted_invoice_url": "https://pay.example/in",
                "customer": "cus_x"},
    "price": {"product": "prod_x", "unit_amount": 900, "currency": "eur", "recurring": {"interval": "month"}},
    "product": {"name": "Pro", "description": None},
    "account": {"business_profile": {"name": "Fixture test"}, "charges_enabled": True, "country": "FR"},
}


def normalise_headers(items) -> list:
    out = []
    for k, v in items:
        lk = k.lower()
        # X-Stripe-Client-Telemetry: the last request of the thread, which in a thread pool is not deterministic
        if lk in ("host", "content-length", "accept-encoding", "x-stripe-client-telemetry"):
            continue
        if lk == "idempotency-key" and UUID4.match(v):
            v = "<uuid4>"
        elif lk == "x-stripe-client-user-agent":
            ua = json.loads(v)
            for f in ("platform", "uname"):
                if f in ua:
                    ua[f] = f"<{f}>"
            v = json.dumps(ua)
        out.append([k, v])
    return sorted(out)


def unflatten(pairs) -> dict:
    """`a[b][0][c]=v` form fields back into JSON (numbers and booleans typed)."""
    root: dict = {}
    for k, v in pairs:
        parts = re.findall(r"[^\[\]]+", k)
        cur = root
        for i, p in enumerate(parts):
            last = i == len(parts) - 1
            if last:
                cur[p] = int(v) if re.fullmatch(r"-?\d+", v) else {"true": True, "false": False}.get(v, v)
            else:
                cur = cur.setdefault(p, {})
    return listify(root)


def listify(d):
    if isinstance(d, dict):
        if d and all(k.isdigit() for k in d):
            return [listify(d[k]) for k in sorted(d, key=int)]
        return {k: listify(v) for k, v in d.items()}
    return d


def rid(*parts) -> str:
    return hashlib.sha1("|".join(parts).encode()).hexdigest()[:12]


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def reply(self, code: int, body, headers=()):
        raw = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def handle_any(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n).decode("utf-8", "replace") if n else ""
        url = urlsplit(self.path)
        form = parse_qsl(body, keep_blank_values=True) + parse_qsl(url.query, keep_blank_values=True)
        echo = {"method": self.command, "path": self.path, "body": body, "headers": normalise_headers(self.headers.items())}
        req_id = "req_" + rid(self.command, url.path, body)
        hdr = [("Request-Id", req_id)]
        text = unquote_plus(self.path) + " " + unquote_plus(body)

        def has(word: str) -> bool:
            # a trigger word on its own (`cus_missing`, `missing@...`), not inside a field name (`missing_payment_method`)
            return re.search(rf"(?<![a-z]){word}(?![a-z_])", text) is not None

        def err(code, payload, extra=()):
            self.reply(code, payload, hdr + list(extra))

        if has("noretry"):
            return err(500, {"error": {"type": "api_error", "message": "Not retried."}}, [("Stripe-Should-Retry", "false")])
        if has("boom"):
            key = self.headers.get("Idempotency-Key") or text
            with LOCK:
                ATTEMPTS[key] = ATTEMPTS.get(key, 0) + 1
                k = ATTEMPTS[key]
            return err(500, {"error": {"type": "api_error", "message": f"Server down (attempt {k})."}})
        if has("missing"):
            return err(404, {"error": {"type": "invalid_request_error", "code": "resource_missing", "param": "id",
                                       "message": "No such customer: 'cus_missing'", "doc_url": "https://stripe.com/docs/error-codes/resource-missing"}})
        if has("declined"):
            return err(402, {"error": {"type": "card_error", "code": "card_declined", "decline_code": "generic_decline",
                                       "param": "payment_method", "message": "Your card was declined."}})
        if has("ratelimit"):
            return err(429, {"error": {"type": "invalid_request_error", "code": "rate_limit", "message": "Too many requests."}},
                       [("Stripe-Should-Retry", "false")])
        if has("badjson"):
            return self.reply(200, b"<html>not json</html>", hdr)
        if has("latin1"):
            return self.reply(200, b"caf\xe9", hdr)
        if has("noerror"):
            return err(400, {"foo": 1})
        if has("oauth"):
            return err(400, {"error": "invalid_grant", "error_description": "The authorization code is invalid."})
        if has("unauth"):
            return err(401, {"error": {"type": "invalid_request_error", "message": "Invalid API Key provided: sk_test_*****"}})
        if has("perm"):
            return err(403, {"error": {"type": "invalid_request_error", "message": "Permission denied."}})
        if has("idem"):
            return err(400, {"error": {"type": "idempotency_error", "message": "Keys for idempotent requests can only be used with the same parameters."}})

        segs = [s for s in url.path.split("/")[2:] if s]
        coll = "/".join(segs[:2]) if len(segs) >= 2 and "/".join(segs[:2]) in OBJECTS else segs[0] if segs else ""
        rest = segs[2:] if coll.count("/") else segs[1:]
        if rest and rest[-1] == "balance_transactions":
            coll, rest = "balance_transactions", []
        obj = OBJECTS.get(coll, coll.rstrip("s"))
        params = unflatten(form)
        if self.command == "GET" and not rest and coll not in ("account",):
            return self.list_page(coll, obj, params, echo, hdr)
        oid = unquote_plus(rest[0]) if rest else f"{obj.split('.')[-1][:3]}_{rid(body, url.path)}"
        out = {"id": oid, "object": obj}
        out.update(json.loads(json.dumps(CANNED.get(obj, {}))))
        for k, v in params.items():
            if k not in ("expand",):
                out[k] = v
        if self.command == "DELETE":
            out["status"] = "canceled"
        if len(rest) > 1:
            out["action"] = rest[1]
        out["echo"] = echo
        self.reply(200, out, hdr)

    def list_page(self, coll, obj, params, echo, hdr):
        total = 5
        limit = int(params.get("limit", 10))
        ids = [f"{obj.split('.')[-1][:3]}_{i}" for i in range(1, total + 1)]
        start = 0
        if "starting_after" in params:
            start = ids.index(params["starting_after"]) + 1 if params["starting_after"] in ids else total
        elif "ending_before" in params:
            end = ids.index(params["ending_before"]) if params["ending_before"] in ids else 0
            start = max(0, end - limit)
            page = ids[start:end]
            return self.reply(200, {"object": "list", "url": f"/v1/{coll}", "has_more": start > 0,
                                    "data": [self.item(obj, i) for i in page], "echo": echo}, hdr)
        page = ids[start:start + limit]
        self.reply(200, {"object": "list", "url": f"/v1/{coll}", "has_more": start + limit < total,
                         "data": [self.item(obj, i) for i in page], "echo": echo}, hdr)

    def item(self, obj, i):
        out = {"id": i, "object": obj}
        out.update(json.loads(json.dumps(CANNED.get(obj, {}))))
        return out

    do_GET = do_POST = do_DELETE = handle_any


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
