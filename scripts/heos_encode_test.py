"""Which URL spelling does the HEOS CLI actually accept?

`heos://player/play_stream?pid=&url=` — try the raw URL, a minimally escaped
one and a fully percent-encoded one, and report the resulting play state.
"""

import socket
import subprocess
import sys
import time

HOST = "192.168.29.45"
PORT = 1255
PID = "1245386843"

raw = subprocess.run(
    [
        "curl",
        "-s",
        "-m",
        "25",
        "https://music.163.com/api/song/enhance/player/url/v1?ids=%5B1330348068%5D&level=standard&encodeType=&csrf_token=",
        "-H",
        "User-Agent: Mozilla/5.0",
        "-H",
        "Referer: https://music.163.com/",
        "-H",
        "Cookie: os=pc",
    ],
    capture_output=True,
    text=True,
)
import json

url = json.loads(raw.stdout)["data"][0]["url"]
print("url:", url[:90], "...")


def full(s):
    return "".join(
        chr(b) if (chr(b).isalnum() or chr(b) in "-_.~") else f"%{b:02X}" for b in s.encode()
    )


def minimal(s):
    # Only what would break the command line itself.
    return s.replace("&", "%26").replace(" ", "%20")


def trial(label, u):
    s = socket.create_connection((HOST, PORT), timeout=8)
    s.settimeout(2.0)
    buf = b""

    def rd(w):
        nonlocal buf
        end = time.time() + w
        out = []
        while time.time() < end:
            try:
                c = s.recv(65536)
            except socket.timeout:
                continue
            except Exception:
                break
            if not c:
                break
            buf += c
            while b"\n" in buf:
                l, buf = buf.split(b"\n", 1)
                if l.strip():
                    out.append(l.decode("utf-8", "replace").strip())
        return out

    rd(0.5)
    s.sendall(f"heos://player/play_stream?pid={PID}&url={u}\r\n".encode())
    lines = rd(4)
    s.sendall(f"heos://player/get_play_state?pid={PID}\r\n".encode())
    lines += rd(2.5)
    state = "?"
    for l in lines:
        if "get_play_state" in l:
            try:
                state = json.loads(l)["heos"]["message"].split("state=")[-1]
            except Exception:
                pass
        if "player_state_changed" in l and "state=" in l:
            state = l.split("state=")[-1].rstrip('"}')
    print(f"{label:10s} -> state={state}")
    s.close()
    time.sleep(1)


trial("raw", url)
trial("minimal", minimal(url))
trial("full", full(url))
