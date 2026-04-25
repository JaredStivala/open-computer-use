#!/usr/bin/env python3
from __future__ import annotations

import base64
import json
import os
import socket
import struct
import sys


def ws_connect(host: str = "127.0.0.1", port: int = 9222, path: str = "/session"):
    key = base64.b64encode(os.urandom(16)).decode()
    sock = socket.create_connection((host, port), timeout=3)
    req = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(req.encode())
    resp = sock.recv(4096)
    if b"101 Switching Protocols" not in resp:
        raise RuntimeError(resp.decode(errors="ignore"))
    return sock


def ws_send(sock, obj) -> None:
    data = json.dumps(obj, separators=(",", ":")).encode()
    mask = os.urandom(4)
    header = bytearray([0x81])
    if len(data) < 126:
        header.append(0x80 | len(data))
    elif len(data) < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", len(data))
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", len(data))
    encoded = bytes(byte ^ mask[i % 4] for i, byte in enumerate(data))
    sock.sendall(header + mask + encoded)


def ws_recv(sock):
    header = sock.recv(2)
    if not header:
        raise RuntimeError("websocket closed")
    _, second = header
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", sock.recv(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", sock.recv(8))[0]
    mask = sock.recv(4) if second & 0x80 else None
    data = b""
    while len(data) < length:
        data += sock.recv(length - len(data))
    if mask:
        data = bytes(byte ^ mask[i % 4] for i, byte in enumerate(data))
    return json.loads(data.decode())


def command(sock, msg_id: int, method: str, params: dict):
    ws_send(sock, {"id": msg_id, "method": method, "params": params})
    while True:
        msg = ws_recv(sock)
        if msg.get("id") == msg_id:
            if msg.get("type") != "success":
                raise RuntimeError(msg)
            return msg["result"]


EXPR = r"""
JSON.stringify((() => {
  const pageTitle = document.title.replace(/ - Wikipedia$/, '').trim().toLowerCase();
  const skipSelf = new Set([pageTitle]);
  if (pageTitle === 'genus') skipSelf.add('genera');
  else if (pageTitle.endsWith('y')) skipSelf.add(pageTitle.slice(0, -1) + 'ies');
  else if (pageTitle) skipSelf.add(pageTitle + 's');

  function isParenthesized(a) {
    const root = a.closest('p');
    if (!root) return false;
    const range = document.createRange();
    range.setStart(root, 0);
    range.setEndBefore(a);
    const before = range.toString();
    let depth = 0;
    for (const ch of before) {
      if (ch === '(') depth++;
      else if (ch === ')' && depth > 0) depth--;
    }
    return depth > 0;
  }

  const links = [...document.querySelectorAll('#mw-content-text .mw-parser-output p a[href^="/wiki/"]')];
  for (const a of links) {
    const text = a.textContent.trim();
    const lower = text.toLowerCase();
    if (!text || text.length > 80) continue;
    if (skipSelf.has(lower)) continue;
    if (lower.startsWith('[') || lower.startsWith('/') || lower.includes('disambiguation')) continue;
    if (text.includes('(') || text.includes(')')) continue;
    if (a.closest('i,em')) continue;
    if (isParenthesized(a)) continue;
    const rect = a.getBoundingClientRect();
    if (!rect.width || !rect.height) continue;
    return {
      text,
      href: a.href,
      title: document.title,
      x: Math.round(window.mozInnerScreenX + rect.left + rect.width / 2),
      y: Math.round(window.mozInnerScreenY + rect.top + rect.height / 2)
    };
  }
  return null;
})())
"""


def main() -> int:
    sock = ws_connect()
    try:
        command(sock, 1, "session.new", {"capabilities": {}})
        tree = command(sock, 2, "browsingContext.getTree", {})
        contexts = tree.get("contexts", [])
        if not contexts:
            return 2
        context = contexts[0]["context"]
        result = command(
            sock,
            3,
            "script.evaluate",
            {
                "expression": EXPR,
                "target": {"context": context},
                "awaitPromise": False,
                "resultOwnership": "none",
            },
        )
        remote = result.get("result", {})
        value = remote.get("value")
        if not value:
            return 3
        if isinstance(value, str):
            value = json.loads(value)
        print(json.dumps(value, separators=(",", ":")))
        return 0
    finally:
        try:
            command(sock, 99, "session.end", {})
        except Exception:
            pass
        sock.close()


if __name__ == "__main__":
    sys.exit(main())
