#!/usr/bin/env python3
import socket
import struct
import sys

import msgpack


def main() -> int:
    sock_path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/agent/capture.sock"
    out_path = sys.argv[2] if len(sys.argv) > 2 else "/tmp/agent/inspect-frame.jpg"
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(sock_path)
    header = read_exact(sock, 4)
    length = struct.unpack(">I", header)[0]
    body = read_exact(sock, length)
    msg = msgpack.unpackb(body, raw=False)
    frame = msg.get("Capture") or {}
    payload = frame.get("bytes", b"")
    if isinstance(payload, list):
        payload = bytes(payload)
    with open(out_path, "wb") as handle:
        handle.write(payload)
    print({"encoding": frame.get("encoding"), "width": frame.get("width"), "height": frame.get("height"), "out": out_path})
    return 0


def read_exact(sock: socket.socket, size: int) -> bytes:
    chunks = []
    remaining = size
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise RuntimeError("socket closed before receiving full message")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


if __name__ == "__main__":
    raise SystemExit(main())
