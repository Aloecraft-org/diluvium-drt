#!/usr/bin/env python3
"""A relay that accepts what it is sent and records it.

Not part of the lesson: a real deployment points `deploy.json` at its own
relay. This exists so the example runs anywhere, with no account and no
network, and so the wire can be shown rather than described.
"""
import socket
import sys

EXPECTED_MESSAGES = int(sys.argv[1]) if len(sys.argv) > 1 else 1

srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 2525))
srv.listen(4)
print("ready", flush=True)

wire = []
for _ in range(EXPECTED_MESSAGES):
    conn, _ = srv.accept()
    io, in_data = conn.makefile("rwb"), False
    io.write(b"220 fake ESMTP\r\n")
    io.flush()
    while True:
        line = io.readline()
        if not line:
            break
        text = line.decode(errors="replace").rstrip("\r\n")
        wire.append(text)
        if in_data:
            if text == ".":
                in_data = False
                io.write(b"250 queued\r\n")
                io.flush()
            continue
        if text.startswith("EHLO"):
            io.write(b"250 fake\r\n")
        elif text.startswith(("MAIL", "RCPT")):
            io.write(b"250 ok\r\n")
        elif text.startswith("DATA"):
            in_data = True
            io.write(b"354 go\r\n")
        elif text.startswith("QUIT"):
            break
        else:
            io.write(b"500 what\r\n")
        io.flush()
    conn.close()
    wire.append("")

with open("wire.txt", "w") as f:
    f.write("\n".join(wire))
