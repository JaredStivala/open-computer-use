#!/usr/bin/env python3
"""Measure the non-model GUI loop on X11.

This probe is intentionally GUI-first: it reads visible controls from the OS
accessibility tree, injects exact clicks through XTest, and waits for visible
window/title changes. It is a diagnostic for the fast controller path, not a
browser automation path.
"""

from __future__ import annotations

import argparse
import ctypes
import subprocess
import sys
import time
from dataclasses import dataclass

import pyatspi


VIEW_W = 1024
VIEW_H = 768


@dataclass
class Element:
    role: str
    name: str
    x: int
    y: int
    w: int
    h: int

    @property
    def cx(self) -> int:
        return self.x + self.w // 2

    @property
    def cy(self) -> int:
        return self.y + self.h // 2


def active_title(display: str) -> str:
    try:
        active = subprocess.check_output(
            ["xprop", "-display", display, "-root", "_NET_ACTIVE_WINDOW"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
        window_id = active.split()[-1]
        title = subprocess.check_output(
            ["xprop", "-display", display, "-id", window_id, "_NET_WM_NAME", "WM_NAME"],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        return ""
    title_line = next((line for line in title.splitlines() if '"' in line), title)
    if '"' not in title_line:
        return title.strip()
    return title_line[title_line.find('"') + 1 : title_line.rfind('"')]


def interactive(role: str) -> bool:
    role = role.lower()
    return (
        "link" in role
        or "button" in role
        or "entry" in role
        or role
        in {
            "tab",
            "menu item",
            "menu button",
            "check box",
            "radio button",
            "combo box",
            "tree item",
            "password text",
            "spin button",
        }
    )


def visible_elements() -> list[Element]:
    out: list[Element] = []

    def walk(obj) -> None:
        try:
            role = obj.getRoleName()
            name = (obj.name or "").strip()
            if interactive(role) and name:
                comp = obj.queryComponent()
                x, y, w, h = comp.getExtents(pyatspi.DESKTOP_COORDS)
                if (
                    w > 0
                    and h > 0
                    and 0 <= x < VIEW_W
                    and 0 <= y < VIEW_H
                    and w * h <= (VIEW_W * VIEW_H) // 4
                ):
                    out.append(Element(role, name, x, y, w, h))
            for i in range(obj.childCount):
                walk(obj[i])
        except Exception:
            return

    desktop = pyatspi.Registry.getDesktop(0)
    for i in range(desktop.childCount):
        walk(desktop[i])
    return out


def article_links(elements: list[Element]) -> list[Element]:
    links = [
        e
        for e in elements
        if "link" in e.role.lower()
        and 340 <= e.y <= 735
        and e.x < 720
        and e.h <= 40
        and not e.name.endswith((".jpg", ".jpeg", ".png", ".svg", ".gif", ".webp"))
        and "disambiguation" not in e.name.lower()
        and not e.name.startswith("Wikipedia")
        and not any(ch in e.name for ch in "\n\r")
        and len(e.name) <= 80
    ]
    links.sort(key=lambda e: (e.y, e.x))
    return links


def xtest_click(display: str, x: int, y: int) -> None:
    x11 = ctypes.cdll.LoadLibrary("libX11.so.6")
    xtst = ctypes.cdll.LoadLibrary("libXtst.so.6")
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.restype = ctypes.c_int
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XFlush.restype = ctypes.c_int
    xtst.XTestFakeMotionEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_ulong,
    ]
    xtst.XTestFakeMotionEvent.restype = ctypes.c_int
    xtst.XTestFakeButtonEvent.argtypes = [
        ctypes.c_void_p,
        ctypes.c_uint,
        ctypes.c_int,
        ctypes.c_ulong,
    ]
    xtst.XTestFakeButtonEvent.restype = ctypes.c_int
    dpy = x11.XOpenDisplay(display.encode())
    if not dpy:
        raise RuntimeError(f"XOpenDisplay failed for {display}")
    try:
        xtst.XTestFakeMotionEvent(dpy, -1, int(x), int(y), 0)
        xtst.XTestFakeButtonEvent(dpy, 1, 1, 0)
        xtst.XTestFakeButtonEvent(dpy, 1, 0, 0)
        x11.XFlush(dpy)
    finally:
        x11.XCloseDisplay(dpy)


def wait_title_change(display: str, before: str, timeout: float) -> tuple[bool, str, float]:
    start = time.perf_counter()
    while time.perf_counter() - start < timeout:
        title = active_title(display)
        if title and title != before:
            return True, title, (time.perf_counter() - start) * 1000
        time.sleep(0.025)
    return False, active_title(display), (time.perf_counter() - start) * 1000


def wait_useful_scene(timeout: float) -> tuple[list[Element], list[Element], float]:
    start = time.perf_counter()
    last_elements: list[Element] = []
    last_links: list[Element] = []
    while time.perf_counter() - start < timeout:
        last_elements = visible_elements()
        last_links = article_links(last_elements)
        if last_links:
            return last_elements, last_links, (time.perf_counter() - start) * 1000
        time.sleep(0.05)
    return last_elements, last_links, (time.perf_counter() - start) * 1000


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--display", default=":99")
    parser.add_argument("--steps", type=int, default=5)
    parser.add_argument("--wait-s", type=float, default=2.0)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    for step in range(1, args.steps + 1):
        observe_start = time.perf_counter()
        title = active_title(args.display)
        elements, links, ready_ms = wait_useful_scene(1.5)
        observe_ms = (time.perf_counter() - observe_start) * 1000
        print(
            {
                "step": step,
                "title": title,
                "observe_ms": round(observe_ms, 1),
                "ready_ms": round(ready_ms, 1),
                "elements": len(elements),
                "article_links": [(e.name, e.cx, e.cy) for e in links[:8]],
            },
            flush=True,
        )
        if "Philosophy - Wikipedia" in title:
            return 0
        if not links:
            print({"step": step, "error": "no article links"}, flush=True)
            return 2
        target = links[0]
        if args.dry_run:
            continue
        input_start = time.perf_counter()
        xtest_click(args.display, target.cx, target.cy)
        input_ms = (time.perf_counter() - input_start) * 1000
        changed, new_title, wait_ms = wait_title_change(args.display, title, args.wait_s)
        print(
            {
                "step": step,
                "clicked": target.name,
                "x": target.cx,
                "y": target.cy,
                "input_ms": round(input_ms, 1),
                "changed": changed,
                "new_title": new_title,
                "wait_ms": round(wait_ms, 1),
            },
            flush=True,
        )
        if not changed:
            return 3
    return 1


if __name__ == "__main__":
    sys.exit(main())
