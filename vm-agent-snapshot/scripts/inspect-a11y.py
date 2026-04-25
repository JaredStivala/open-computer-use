#!/usr/bin/env python3
import socket
import struct
import sys

import msgpack


def main() -> int:
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/agent/a11y.sock"
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(path)
    header = read_exact(sock, 4)
    length = struct.unpack(">I", header)[0]
    body = read_exact(sock, length)
    msg = msgpack.unpackb(body, raw=False)
    print(msg)
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
