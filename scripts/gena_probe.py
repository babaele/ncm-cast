"""Listen for GENA NOTIFY callbacks so we can prove the renderer can reach us.

If a SUBSCRIBE with `CALLBACK: <http://<us>:<port>/evt/x>` produces no NOTIFY,
the renderer cannot open a connection back to this host (firewall / AP
isolation), and no URL we hand it will ever be fetched.
"""

import socket
import sys
import threading

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9113

srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", PORT))
srv.listen(8)
print(f"GENA probe listening on 0.0.0.0:{PORT}", flush=True)


def serve(c, addr):
    c.settimeout(5)
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            chunk = c.recv(4096)
            if not chunk:
                break
            buf += chunk
    except Exception:
        pass
    print(f"### NOTIFY from {addr}:", flush=True)
    print(buf.decode("utf-8", "replace"), flush=True)
    try:
        c.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    except Exception:
        pass
    c.close()


while True:
    try:
        conn, a = srv.accept()
    except OSError:
        break
    threading.Thread(target=serve, args=(conn, a), daemon=True).start()
