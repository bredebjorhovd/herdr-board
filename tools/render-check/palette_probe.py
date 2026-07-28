#!/usr/bin/env python3
"""Ask the terminal we are running inside for its real colours.

OSC 4 gives the 16 ANSI slots, OSC 10/11 the default foreground and background.
Inside a herdr pane these answers come from herdr's active theme, so this is the
theme's palette measured rather than guessed.

Usage: palette_probe.py <out.json>
"""
import json
import os
import re
import select
import sys
import termios
import tty

QUERIES = [(f"ansi{i}", f"\x1b]4;{i};?\x07") for i in range(16)] + [
    ("fg", "\x1b]10;?\x07"),
    ("bg", "\x1b]11;?\x07"),
]

RGB = re.compile(r"rgb:([0-9a-fA-F]+)/([0-9a-fA-F]+)/([0-9a-fA-F]+)")


def to_hex(match):
    def scale(component):
        # Terminals answer in 16-bit-per-channel form; take the high byte.
        return int(component[:2], 16)

    return "#%02x%02x%02x" % tuple(scale(c) for c in match.groups())


def main():
    tty_fd = os.open("/dev/tty", os.O_RDWR)
    saved = termios.tcgetattr(tty_fd)
    out = {}
    try:
        tty.setraw(tty_fd)
        for name, query in QUERIES:
            os.write(tty_fd, query.encode())
            buf = b""
            # Answers arrive in one burst; 400ms is generous for a local pane.
            while select.select([tty_fd], [], [], 0.4)[0]:
                buf += os.read(tty_fd, 256)
                if buf.endswith(b"\x07") or buf.endswith(b"\x1b\\"):
                    break
            m = RGB.search(buf.decode("utf-8", "replace"))
            out[name] = to_hex(m) if m else None
    finally:
        termios.tcsetattr(tty_fd, termios.TCSADRAIN, saved)
        os.close(tty_fd)

    with open(sys.argv[1], "w") as f:
        json.dump(out, f, indent=2)
    missing = [k for k, v in out.items() if v is None]
    print(f"wrote {sys.argv[1]}; unanswered: {missing or 'none'}")


main()
