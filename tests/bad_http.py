"""A misbehaving HTTP/1.1 server for the outbound httpx error cases (fixtures/integapp/httpxerr.py): the request's
path picks the misbehaviour. `python tests/bad_http.py <port>`."""
import socket
import struct
import sys
import threading
import time


def handle(c: socket.socket) -> None:
    data = b""
    while b"\r\n\r\n" not in data:
        d = c.recv(65536)
        if not d:
            c.close()
            return
        data += d
    path = data.split(b" ")[1].decode()
    if path.startswith("/close"):  # closed before any response
        pass
    elif path.startswith("/partial"):  # 5 of the 10 bytes announced
        c.sendall(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 10\r\n\r\nhello")
    elif path.startswith("/chunked"):  # a chunk, then no terminating chunk
        c.sendall(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n")
    elif path.startswith("/reset"):  # RST instead of FIN
        c.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    elif path.startswith("/slow"):  # no answer within the client's timeout
        time.sleep(3)
    elif path.startswith("/s503"):
        c.sendall(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 4\r\nconnection: close\r\n\r\nbusy")
    else:
        c.sendall(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
    c.close()


def main() -> None:
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", int(sys.argv[1])))
    s.listen(64)
    while True:
        c, _ = s.accept()
        threading.Thread(target=handle, args=(c,), daemon=True).start()


if __name__ == "__main__":
    main()
