"""A local SMTP sink: accepts every message (no TLS, no AUTH) and appends its envelope to a log file, so a
conformance pass can send e-mail without anything leaving the machine; with a mail directory, each message is
also kept whole (`<dir>/<n>.eml`), for a scenario that follows a link sent by e-mail (`last_mail`).
Usage: python tests/smtp_sink.py <port> [log file] [mail directory]"""
import email
import email.policy
import itertools
import os
import re
import socketserver
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


LOG = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] != "-" else None
MAILDIR = sys.argv[3] if len(sys.argv) > 3 else None
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


if __name__ == "__main__":
    Server(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
