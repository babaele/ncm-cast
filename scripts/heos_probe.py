"""Raw HEOS telnet probe: enumerate players and push a URL, print every reply.

Uses a hand-rolled line buffer: `socket.makefile()` becomes unusable after a
read timeout, which would make every later command look like a failure.
"""

import json
import socket
import sys
import time

HOST = sys.argv[1] if len(sys.argv) > 1 else "192.168.29.45"
PORT = 1255
URL = sys.argv[2] if len(sys.argv) > 2 else "http://192.168.29.44:9112/probe.mp3"

s = socket.create_connection((HOST, PORT), timeout=10)
s.settimeout(2.0)
buf = b""


def read_json(wait):
    """Pull complete JSON lines off the socket for up to `wait` seconds."""
    global buf
    end = time.time() + wait
    out = []
    while time.time() < end:
        try:
            chunk = s.recv(65536)
        except socket.timeout:
            pass
        except Exception as e:
            print("recv err", e, flush=True)
            break
        else:
            if not chunk:
                print("(socket closed by peer)", flush=True)
                break
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                line = line.decode("utf-8", "replace").strip()
                if line:
                    out.append(line)
        # let a partially buffered line settle
        if buf.strip():
            try:
                json.loads(buf.decode("utf-8", "replace"))
            except Exception:
                pass
            else:
                out.append(buf.decode("utf-8", "replace").strip())
                buf = b""
    if buf.strip():
        out.append(buf.decode("utf-8", "replace").strip())
        buf = b""
    return out


def cmd(c, wait=3.0):
    print(f">>> {c}", flush=True)
    try:
        s.sendall((c + "\r\n").encode())
    except Exception as e:
        print("send err", e, flush=True)
        return []
    for line in read_json(wait):
        print("   ", line[:400], flush=True)
    return []


def find_pid():
    global buf
    for _ in range(3):
        pass
    return None


players = []
print(">>> heos://player/get_players", flush=True)
s.sendall(b"heos://player/get_players\r\n")
for line in read_json(3):
    print("   ", line[:400], flush=True)
    players.append(line)

pid = None
for l in players:
    try:
        v = json.loads(l)
    except Exception:
        continue
    pl = v.get("payload")
    if isinstance(pl, list) and pl and "pid" in pl[0]:
        pid = pl[0]["pid"]
        print("player:", pid, pl[0].get("name"), flush=True)

if pid:
    cmd("heos://system/register_for_change_events?enable=on", 3)
    cmd(f"heos://player/play_stream?pid={pid}&url={URL}", 4)
    cmd(f"heos://player/get_play_state?pid={pid}", 3)

print("--- waiting for the renderer to fetch the URL ---", flush=True)
time.sleep(8)
try:
    s.close()
except Exception:
    pass
