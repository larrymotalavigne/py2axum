"""A local SMTP sink: accepts every message (no TLS, no AUTH) and appends its envelope to a log file, so a
conformance pass can send e-mail without anything leaving the machine.
Usage: python tests/smtp_sink.py <port> [log file]"""
import socketserver
import sys


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
                size = 0
                while (data := self.rfile.readline()) not in (b".\r\n", b".\n", b""):
                    size += len(data)
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


LOG = sys.argv[2] if len(sys.argv) > 2 else None

if __name__ == "__main__":
    Server(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
