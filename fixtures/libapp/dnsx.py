"""dnspython 2.8's stub resolver against tests/dns_stub.py (DNS_STUB_PORT): answers (MX, TXT, A, AAAA, NS, CNAME
chains), the rdata texts, NODATA/NXDOMAIN/error rcodes and their exceptions, silence (timeouts, lifetime), truncation
(TCP), a wrong id, trailing junk, failover between nameservers; Resolver() read from /etc/resolv.conf and from the
fixture's resolv.conf files; the module's resolve() on the default resolver (what a mail service calls). Durations in the
messages are masked (they depend on the clock), never the rest."""
import os
import re
from pathlib import Path

import dns.exception
import dns.name
import dns.resolver
from fastapi import APIRouter

router = APIRouter(prefix="/dnsx")
STUB_PORT = int(os.getenv("DNS_STUB_PORT", "8599"))
# a UDP port nobody answers on (a nameserver that times out)
DEAD_PORT = int(os.getenv("DNS_DEAD_PORT", "8598"))
CONF_DIR = Path(__file__).parent / "resolv"


def _mask(text: str) -> str:
    return re.sub(r"\d+\.\d{3} seconds", "N seconds", text)


def _err(e: Exception) -> dict:
    return {"err": type(e).__name__, "msg": _mask(str(e)).replace(str(CONF_DIR), "<conf>"), "dns": isinstance(e, dns.exception.DNSException),
            "timeout": isinstance(e, dns.exception.Timeout), "lifetime": isinstance(e, dns.resolver.LifetimeTimeout),
            "resolver_timeout": isinstance(e, dns.resolver.Timeout), "form": isinstance(e, dns.exception.FormError),
            "syntax": isinstance(e, dns.exception.SyntaxError), "value": isinstance(e, ValueError)}


def _rdata(r) -> dict:
    out = {"type": type(r).__name__, "text": r.to_text(), "str": str(r), "repr": repr(r)}
    if out["type"] == "MX":
        out["preference"] = r.preference
        out["exchange"] = str(r.exchange)
        out["exchange_short"] = r.exchange.to_text(omit_final_dot=True)
        out["host"] = str(r.exchange).rstrip(".")
    if out["type"] == "TXT":
        out["strings"] = [s.decode("latin-1") for s in r.strings]
        try:
            out["joined"] = "".join([s.decode("utf-8") if isinstance(s, bytes) else s for s in r.strings])
        except UnicodeDecodeError as e:
            out["joined"] = str(e)
    if out["type"] in ("A", "AAAA"):
        out["address"] = r.address
    if out["type"] in ("CNAME", "NS"):
        out["target"] = str(r.target)
    return out


def _resolver(body: dict) -> dns.resolver.Resolver:
    r = dns.resolver.Resolver(configure=False)
    r.nameservers = body.get("nameservers", ["127.0.0.1"])
    if "ports" in body:
        r.nameserver_ports = {k: (STUB_PORT if v == "stub" else DEAD_PORT) for k, v in body["ports"].items()}
    r.port = DEAD_PORT if body.get("port") == "dead" else STUB_PORT
    r.timeout = body.get("timeout", 2.0)
    r.lifetime = body.get("lifetime", 5.0)
    if body.get("edns"):
        r.use_edns(0, 0, 1232)
    if body.get("retry_servfail"):
        r.retry_servfail = True
    return r


@router.post("/resolve")
async def resolve(body: dict):
    r = _resolver(body)
    kw = {}
    for k in ("tcp", "raise_on_no_answer", "lifetime", "search"):
        if k in body.get("kw", {}):
            kw[k] = body["kw"][k]
    try:
        if "rdtype" in body:
            answer = r.resolve(body["qname"], body["rdtype"], **kw)
        else:
            answer = r.resolve(body["qname"], **kw)
    except Exception as e:
        return _err(e)
    items = [_rdata(x) for x in answer]
    out = {"answers": items, "len": len(answer), "bool": bool(answer), "qname": str(answer.qname),
           "canonical": str(answer.canonical_name)}
    if len(answer):
        out["first"] = _rdata(answer[0])
    return out


@router.get("/rdtypes")
async def rdtypes(qname: str, rdtype: str):
    r = _resolver({})
    try:
        return {"n": len(r.resolve(qname, rdtype))}
    except Exception as e:
        return _err(e)


@router.get("/system")
async def system():
    """Resolver() as /etc/resolv.conf sets it up, and the default resolver"""
    try:
        r = dns.resolver.Resolver()
    except Exception as e:
        return _err(e)
    d = dns.resolver.get_default_resolver()
    return {"nameservers": list(r.nameservers), "search": [str(s) for s in r.search], "domain": str(r.domain),
            "timeout": r.timeout, "lifetime": r.lifetime, "port": r.port, "ndots": r.ndots, "rotate": r.rotate,
            "edns": r.edns, "payload": r.payload, "default_same": list(d.nameservers) == list(r.nameservers),
            "use_search_by_default": r.use_search_by_default, "retry_servfail": r.retry_servfail}


@router.get("/conf/{name}")
async def conf(name: str):
    try:
        r = dns.resolver.Resolver(filename=str(CONF_DIR / name))
    except Exception as e:
        return _err(e)
    return {"nameservers": r.nameservers, "search": [str(s) for s in r.search], "domain": str(r.domain),
            "timeout": r.timeout, "ndots": r.ndots, "rotate": r.rotate, "edns": r.edns, "payload": r.payload}


@router.get("/unconfigured")
async def unconfigured():
    r = dns.resolver.Resolver(configure=False)
    out = {"nameservers": r.nameservers, "timeout": r.timeout, "lifetime": r.lifetime, "port": r.port,
           "search": r.search, "ndots": r.ndots, "domain": str(r.domain)}
    try:
        r.resolve("mx.p2a.test", "MX")
    except Exception as e:
        out["empty"] = _err(e)
    for bad in (["not an address"], "127.0.0.1", ("192.0.2.1", "::1"), ["2001:db8::1", "10.0.0.1"]):
        try:
            r.nameservers = bad
            out.setdefault("set", []).append("ok")
        except Exception as e:
            out.setdefault("set", []).append(_err(e))
    return out


@router.get("/names")
async def names(text: str, relative: bool = False):
    """dns.name.from_text and Name's text forms"""
    try:
        n = dns.name.from_text(text, None) if relative else dns.name.from_text(text)
    except Exception as e:
        return _err(e)
    return {"str": str(n), "repr": repr(n), "short": n.to_text(True), "abs": n.is_absolute(),
            "eq_root": n == dns.name.root, "eq_upper": n == dns.name.from_text(text.upper(), None if relative else dns.name.root)}


@router.get("/badnames")
async def badnames(text: str):
    """resolve() of names dnspython refuses before any query"""
    r = _resolver({})
    try:
        r.resolve(text, "TXT")
        return {"ok": True}
    except Exception as e:
        return _err(e)


@router.get("/module")
async def module(qname: str, rdtype: str = "MX", raise_on_no_answer: bool = True):
    """dns.resolver.resolve() on the default resolver, pointed at the stub (a mail service's shape)"""
    d = dns.resolver.get_default_resolver()
    d.nameservers = ["127.0.0.1"]
    d.port = STUB_PORT
    d.timeout = 1.0
    d.lifetime = 2.0
    try:
        answers = dns.resolver.resolve(qname, rdtype, raise_on_no_answer=raise_on_no_answer)
        if not answers:
            return {"texts": []}
        return {"texts": [rdata.to_text() for rdata in answers], "hosts": [str(mx.exchange) for mx in answers] if rdtype == "MX" else []}
    except Exception as e:
        return _err(e)


@router.get("/verify")
async def verify(domain: str):
    """a mail service's DNS verification: MX host, SPF include, DKIM key, DMARC"""
    resolver = _resolver({})
    resolver.timeout = 2.0
    resolver.lifetime = 2.0
    out = {}
    try:
        mx_records = resolver.resolve(domain, "MX")
        out["mx"] = [[str(mx.exchange).rstrip("."), mx.preference] for mx in mx_records]
    except dns.resolver.NoAnswer:
        out["mx"] = "noanswer"
    except dns.resolver.NXDOMAIN:
        out["mx"] = "nxdomain"
    except dns.exception.Timeout:
        out["mx"] = "timeout"
    except Exception as e:
        out["mx"] = f"error: {e}"
    for label, name in (("spf", domain), ("dkim", f"default._domainkey.{domain}"), ("dmarc", f"_dmarc.{domain}")):
        try:
            records = resolver.resolve(name, "TXT")
            out[label] = ["".join([s.decode("utf-8") if isinstance(s, bytes) else s for s in rec.strings]) for rec in records]
            out[label + "_str"] = [str(txt) for txt in records if "spf1" in str(txt).lower() or "DKIM1" in str(txt)]
        except dns.resolver.NoAnswer:
            out[label] = "noanswer"
        except dns.resolver.NXDOMAIN:
            out[label] = f"nxdomain {name}"
        except dns.exception.Timeout:
            out[label] = "timeout"
        except Exception as e:
            out[label] = f"error: {_mask(str(e))}"
    return out


@router.get("/classes")
async def classes():
    return {"timeout_alias": dns.resolver.Timeout is dns.resolver.LifetimeTimeout,
            "lifetime_is_timeout": issubclass(dns.resolver.LifetimeTimeout, dns.exception.Timeout),
            "nx_dns": issubclass(dns.resolver.NXDOMAIN, dns.exception.DNSException),
            "nx_exc": issubclass(dns.resolver.NXDOMAIN, Exception),
            "noanswer_dns": issubclass(dns.resolver.NoAnswer, dns.exception.DNSException),
            "empty_label_syntax": issubclass(dns.name.EmptyLabel, dns.exception.SyntaxError),
            "too_long_form": issubclass(dns.name.NameTooLong, dns.exception.FormError)}
