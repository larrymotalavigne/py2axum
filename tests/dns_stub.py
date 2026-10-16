"""A deterministic DNS server for the dnspython conformance (fixtures/libapp/dnsx.py): UDP and TCP on 127.0.0.1:<port>,
one behaviour per name under p2a.test. (answers, CNAME chains, NODATA, NXDOMAIN, error rcodes, silence, truncation,
a wrong id, trailing junk...). Written with struct, not dnspython, so that it can also send what dnspython refuses.

usage: python tests/dns_stub.py <port>
"""
import asyncio
import struct
import sys

ZONE = "p2a.test."
SOA = ("p2a.test.", 6, b"")  # rdata filled by soa()


def name_wire(name: str) -> bytes:
    out = b""
    for label in name.rstrip(".").split("."):
        if label:
            b = label.encode()
            out += bytes([len(b)]) + b
    return out + b"\x00"


def rr(name: str, rdtype: int, rdata: bytes, ttl: int = 300) -> bytes:
    return name_wire(name) + struct.pack("!HHIH", rdtype, 1, ttl, len(rdata)) + rdata


def mx(pref: int, host: str) -> bytes:
    return struct.pack("!H", pref) + name_wire(host)


def txt(*strings: bytes) -> bytes:
    return b"".join(bytes([len(s)]) + s for s in strings)


def soa() -> bytes:
    return name_wire("ns." + ZONE) + name_wire("admin." + ZONE) + struct.pack("!IIIII", 1, 3600, 600, 86400, 60)


def parse_query(data: bytes):
    qid, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", data[:12])
    i, labels = 12, []
    while data[i]:
        n = data[i]
        labels.append(data[i + 1:i + 1 + n])
        i += 1 + n
    i += 1
    qtype, qclass = struct.unpack("!HH", data[i:i + 4])
    qname_raw = data[12:i]
    has_opt = ar > 0
    return qid, flags, qname_raw, b".".join(labels).decode("ascii", "replace").lower() + ".", qtype, qclass, has_opt, data[12:i + 4]


def answer_for(qname: str, qtype: int, has_opt: bool, tcp: bool):
    """(rcode, answers, authority, extra flags, options) for the query; None: no reply"""
    n = qname[: -len(ZONE)] if qname.endswith(ZONE) else None
    soa_rr = [rr(ZONE, 6, soa())]
    if n is None:
        return 5, [], [], 0, {}
    if n == "timeout.":
        return None
    if n == "mx.":
        if qtype != 15:
            return 0, [], soa_rr, 0, {}
        return 0, [rr(qname, 15, mx(10, "mail.p2a.test.")), rr(qname, 15, mx(20, "Backup.P2A.test.")),
                   rr(qname, 15, mx(10, "mail.p2a.test.")), rr(qname, 15, mx(30, "mail.mailer.example."))], [], 0, {}
    if n == "txt.":
        if qtype != 16:
            return 0, [], soa_rr, 0, {}
        return 0, [rr(qname, 16, txt(b"v=spf1 include:mailer.example ~all")), rr(qname, 16, txt(b"v=DKIM1; k=rsa; p=MIIB", b"IjANBg")),
                   rr(qname, 16, txt(b'quote"back\\slash\x01\x7f\xff tab\t')), rr(qname, 16, txt(b"")),
                   rr(qname, 16, txt(b"x" * 255, b"y" * 10))], [], 0, {}
    if n == "_dmarc.":
        return 0, [rr(qname, 16, txt(b"v=DMARC1; p=none"))], [], 0, {}
    if n == "default._domainkey.":
        return 0, [rr(qname, 16, txt(b"v=DKIM1; k=rsa; p=", b"MIIBIjAN BgkqhkiG"))], [], 0, {}
    if n == "cname.":
        target = "txt." + ZONE
        recs = [rr(qname, 5, name_wire(target))]
        if qtype == 16:
            recs.append(rr(target, 16, txt(b"via cname")))
        return 0, recs, soa_rr, 0, {}
    if n == "dangling.":
        return 0, [rr(qname, 5, name_wire("nowhere." + ZONE))], soa_rr, 0, {}
    if n == "loop.":
        recs = []
        names = [f"l{i}.{ZONE}" for i in range(20)]
        recs.append(rr(qname, 5, name_wire(names[0])))
        for a, b in zip(names, names[1:]):
            recs.append(rr(a, 5, name_wire(b)))
        return 0, recs, [], 0, {}
    if n == "nodata.":
        return 0, [], soa_rr, 0, {}
    if n == "nxanswer.":
        return 3, [rr(qname, 16, txt(b"should not be here"))], soa_rr, 0, {}
    if n == "servfail.":
        return 2, [], [], 0, {}
    if n == "refused.":
        return 5, [], [], 0, {}
    if n == "formerr.":
        return 1, [], [], 0, {}
    if n == "notimp.":
        return 4, [], [], 0, {"noquestion": True}
    if n == "yx.":
        return 6, [], [], 0, {}
    if n == "rcode12.":
        return 12, [], [], 0, {}
    if n == "tc.":
        if not tcp:
            return 0, [], [], 0x0200, {}
        return 0, [rr(qname, 16, txt(b"over tcp"))], [], 0, {}
    if n == "tcfail.":
        if not tcp:
            return 0, [], [], 0x0200, {}
        return None
    if n == "badid.":
        return 0, [rr(qname, 16, txt(b"right id"))], [], 0, {"badid_first": True}
    if n == "junk.":
        return 0, [rr(qname, 16, txt(b"junk"))], [], 0, {"junk": True}
    if n == "case.":
        return 0, [rr(qname.upper(), 15, mx(5, "MiXeD.Case.P2A.Test."))], [], 0, {"upper_question": True}
    if n == "a.":
        if qtype == 1:
            return 0, [rr(qname, 1, bytes([192, 0, 2, 1])), rr(qname, 1, bytes([192, 0, 2, 2]))], [], 0, {}
        if qtype == 28:
            return 0, [rr(qname, 28, bytes.fromhex("20010db8000000000000000000000001")),
                       rr(qname, 28, bytes.fromhex("00000000000000000000ffffc0000201")),
                       rr(qname, 28, bytes.fromhex("fe800000000000000202b3fffe1e8329"))], [], 0, {}
        return 0, [], soa_rr, 0, {}
    if n == "ns.":
        return 0, [rr(qname, 2, name_wire("ns1.p2a.test.")), rr(qname, 2, name_wire("ns2.p2a.test."))], [], 0, {}
    if n == "edns.":
        return 0, [rr(qname, 16, txt(b"edns=%s" % (b"yes" if has_opt else b"no")))], [], 0, {}
    if n == "extra.":
        # an SRV record (a type the binary skips) in the answer and the additional section
        srv = struct.pack("!HHH", 1, 2, 3) + name_wire("srv.p2a.test.")
        return 0, [rr(qname, 16, txt(b"with extras")), rr(qname, 33, srv)], [], 0, {"additional": [rr(qname, 33, srv)]}
    if n == "escape.":
        return 0, [rr(qname, 15, mx(1, 'we"ird.(name);@$.p2a.test.'.replace('"', '\\"')))], [], 0, {"raw_mx": True}
    if n == "slow.":
        return 0, [rr(qname, 16, txt(b"slow"))], [], 0, {"delay": 0.6}
    return 3, [], soa_rr, 0, {}


def build(qid: int, flags_in: int, question: bytes, qname: str, qtype: int, has_opt: bool, tcp: bool):
    res = answer_for(qname, qtype, has_opt, tcp)
    if res is None:
        return None, {}
    rcode, answers, authority, extra_flags, opts = res
    flags = 0x8000 | (flags_in & 0x0100) | 0x0080 | extra_flags | rcode
    q = question
    if opts.get("upper_question"):
        q = name_wire(qname.upper()) + question[-4:]
    if opts.get("noquestion"):
        q = b""
    if opts.get("raw_mx"):
        # a name with a label holding the escaped characters dnspython writes back with backslashes
        label = b'we"ird'
        raw = struct.pack("!H", 1) + bytes([len(label)]) + label + bytes([6]) + b"(name)" + bytes([3]) + b";@$" + name_wire(ZONE)
        answers = [name_wire(qname) + struct.pack("!HHIH", 15, 1, 300, len(raw)) + raw]
    additional = opts.get("additional", [])
    body = struct.pack("!HHHHHH", qid, flags, 1 if q else 0, len(answers), len(authority), len(additional))
    body += q + b"".join(answers) + b"".join(authority) + b"".join(additional)
    if opts.get("junk"):
        body += b"\x00junk"
    return body, opts


class Udp(asyncio.DatagramProtocol):
    def connection_made(self, transport):
        self.transport = transport

    def datagram_received(self, data, addr):
        try:
            qid, flags, _raw, qname, qtype, _c, has_opt, question = parse_query(data)
        except Exception:
            return
        body, opts = build(qid, flags, question, qname, qtype, has_opt, False)
        if body is None:
            return
        loop = asyncio.get_running_loop()
        if opts.get("badid_first"):
            self.transport.sendto(struct.pack("!H", (qid + 1) & 0xFFFF) + body[2:], addr)
            loop.call_later(0.05, self.transport.sendto, body, addr)
            return
        if opts.get("delay"):
            loop.call_later(opts["delay"], self.transport.sendto, body, addr)
            return
        self.transport.sendto(body, addr)


async def tcp_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    try:
        while True:
            head = await reader.readexactly(2)
            data = await reader.readexactly(struct.unpack("!H", head)[0])
            qid, flags, _raw, qname, qtype, _c, has_opt, question = parse_query(data)
            body, _opts = build(qid, flags, question, qname, qtype, has_opt, True)
            if body is None:
                break
            writer.write(struct.pack("!H", len(body)) + body)
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    finally:
        writer.close()


async def main(port: int):
    loop = asyncio.get_running_loop()
    await loop.create_datagram_endpoint(Udp, local_addr=("127.0.0.1", port))
    server = await asyncio.start_server(tcp_client, "127.0.0.1", port)
    print(f"dns stub on 127.0.0.1:{port}", flush=True)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main(int(sys.argv[1])))
