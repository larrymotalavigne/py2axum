"""A local SMTP sink: accepts every message (no TLS, no AUTH) and appends its envelope to a log file, so a
conformance pass can send e-mail without anything leaving the machine; with a mail directory, each message is
also kept whole (`<dir>/<n>.eml`), for a scenario that follows a link sent by e-mail (`last_mail`).
Usage: python tests/smtp_sink.py <port> [log file] [mail directory] [--tls]

--tls: implicit TLS (port 465 style) with a throw-away self-signed certificate for a name that is not the one dialled
(`mail.other.invalid`): a sender that checks the chain or the host name must refuse it (aiosmtplib `validate_certs`).
"""
import email
import email.policy
import itertools
import os
import re
import socketserver
import ssl
import tempfile
import sys
import threading
import time


class Handler(socketserver.StreamRequestHandler):
    def reply(self, line: str) -> None:
        self.wfile.write(line.encode() + b"\r\n")

    def handle(self) -> None:
        self.reply("220 sink ESMTP")
        envelope: list[str] = []
        while line := self.rfile.readline():
            cmd = line.decode(errors="replace").strip()
            verb = cmd[:4].upper()
            if verb in {"EHLO", "HELO"}:
                self.wfile.write(b"250-sink\r\n250-8BITMIME\r\n250 SMTPUTF8\r\n" if verb == "EHLO" else b"250 sink\r\n")
            elif verb in {"MAIL", "RCPT"}:
                envelope.append(cmd)
                self.reply("250 OK")
            elif verb == "DATA":
                self.reply("354 end with .")
                size, raw = 0, []
                while (data := self.rfile.readline()) not in (b".\r\n", b".\n", b""):
                    size += len(data)
                    raw.append(data[1:] if data.startswith(b"..") else data)
                if MAILDIR:
                    with LOCK:
                        n = next(COUNTER)
                    with open(os.path.join(MAILDIR, f"{time.time_ns()}-{n:06d}.eml"), "wb") as f:
                        f.write(b"".join(raw))
                if LOG:
                    with open(LOG, "a") as f:
                        f.write(f"{' '.join(envelope)} size={size}\n")
                envelope.clear()
                self.reply("250 queued")
            elif verb == "QUIT":
                self.reply("221 bye")
                return
            else:  # RSET, NOOP, STARTTLS (refused: the sender must not ask for it)
                self.reply("250 OK" if verb in {"RSET", "NOOP"} else "502 not implemented")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


ARGS = [a for a in sys.argv[1:] if a != "--tls"]
TLS = "--tls" in sys.argv
LOG = ARGS[1] if len(ARGS) > 1 and ARGS[1] != "-" else None
MAILDIR = ARGS[2] if len(ARGS) > 2 else None
LOCK, COUNTER = threading.Lock(), itertools.count()


def mails_to(maildir: str, to: str) -> list:
    """The messages kept for `to`, oldest first."""
    out = []
    for name in sorted(os.listdir(maildir)) if os.path.isdir(maildir) else []:
        with open(os.path.join(maildir, name), "rb") as f:
            msg = email.message_from_binary_file(f, policy=email.policy.default)
        if to.lower() in str(msg.get("To", "")).lower():
            out.append(msg)
    return out


def last_mail(maildir: str, to: str, pattern: str, nth: int, timeout: float = 10.0) -> str:
    """The first match of `pattern` (its group 1 if any) in the text of the `nth` message (from 1) sent to `to`
    since the directory was emptied, waiting for it up to `timeout` seconds (a server may send after answering)."""
    deadline = time.time() + timeout
    while len(msgs := mails_to(maildir, to)) < nth:
        if time.time() > deadline:
            raise LookupError(f"{len(msgs)} message(s) to {to} in {maildir}, expected at least {nth}")
        time.sleep(0.1)
    text = "\n".join(part.get_content() for part in msgs[nth - 1].walk() if part.get_content_maintype() == "text")
    m = re.search(pattern, text)
    if not m:
        raise LookupError(f"message {nth} to {to}: no match for {pattern!r}")
    return m.group(1) if m.groups() else m.group(0)


def self_signed_context() -> ssl.SSLContext:
    """A server context with a self-signed certificate for `mail.other.invalid` (valid for a day)."""
    import datetime

    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "mail.other.invalid")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
            .serial_number(x509.random_serial_number()).not_valid_before(now - datetime.timedelta(minutes=5))
            .not_valid_after(now + datetime.timedelta(days=1))
            .add_extension(x509.SubjectAlternativeName([x509.DNSName("mail.other.invalid")]), critical=False)
            .sign(key, hashes.SHA256()))
    with tempfile.TemporaryDirectory() as d:
        pem, keyf = os.path.join(d, "c.pem"), os.path.join(d, "k.pem")
        with open(pem, "wb") as f:
            f.write(cert.public_bytes(serialization.Encoding.PEM))
        with open(keyf, "wb") as f:
            f.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(pem, keyf)
    return ctx


class TlsServer(Server):
    def get_request(self):
        sock, addr = super().get_request()
        sock.settimeout(10)
        try:
            return self.context.wrap_socket(sock, server_side=True), addr
        except OSError:  # a sender that refuses the certificate hangs up during the handshake
            sock.close()
            raise


if __name__ == "__main__":
    if TLS:
        srv = TlsServer(("127.0.0.1", int(ARGS[0])), Handler, bind_and_activate=False)
        srv.allow_reuse_address = True
        srv.context = self_signed_context()
        srv.server_bind()
        srv.server_activate()
        srv.serve_forever()
    Server(("127.0.0.1", int(ARGS[0])), Handler).serve_forever()
