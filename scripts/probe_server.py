"""Throwaway probe: point a renderer at this port and see whether it fetches.

Prints every request line + header block so we can tell exactly what the
renderer asks for (Range? HEAD? Icy-MetaData?) even when the real daemon log
is not available.
"""

import socket
import sys
import threading

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9112
PAYLOAD = b"ID3\x04" + b"\x00" * (64 * 1024)

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", PORT))
srv.listen(8)
print(f"probe listening on 0.0.0.0:{PORT}", flush=True)


def serve(c, addr):
    c.settimeout(8)
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            chunk = c.recv(4096)
            if not chunk:
                break
            buf += chunk
    except Exception:
        pass
    print(f"--- request from {addr} ---", flush=True)
    print(buf.decode("utf-8", "replace"), flush=True)
    head = buf.decode("utf-8", "replace").splitlines()
    method = head[0].split(" ")[0] if head else "?"
    rng = next((l for l in head if l.lower().startswith("range:")), None)
    if rng and "bytes=" in rng:
        spec = rng.split("bytes=", 1)[1].strip()
        start = int(spec.split("-")[0] or 0)
        body = PAYLOAD[start:]
        status = "206 Partial Content"
        extra = f"Content-Range: bytes {start}-{start + len(body) - 1}/{len(PAYLOAD)}\r\n"
    else:
        body = PAYLOAD
        status = "200 OK"
        extra = ""
    hdr = (
        f"HTTP/1.1 {status}\r\n"
        "Content-Type: audio/mpeg\r\n"
        f"Content-Length: {len(body)}\r\n"
        "Accept-Ranges: bytes\r\n"
        "Connection: close\r\n" + extra + "\r\n"
    )
    try:
        if method == "HEAD":
            c.sendall(hdr.encode())
        else:
            c.sendall(hdr.encode() + body)
    except Exception as e:
        print("send err", e, flush=True)
    c.close()


while True:
    try:
        conn, a = srv.accept()
    except OSError:
        break
    threading.Thread(target=serve, args=(conn, a), daemon=True).start()
