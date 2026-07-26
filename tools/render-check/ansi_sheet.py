#!/usr/bin/env python3
"""Turn captured pane ANSI into a contact sheet I can actually look at.

Reads the raw SGR stream the board emits (`herdr pane read --format ansi`) and
re-renders it under a named 16-colour palette, so the same capture can be
inspected on a dark terminal, a light one, and a colourless one.
"""
import html
import json
import re
import sys
from pathlib import Path

SGR = re.compile(r"\x1b\[([0-9;]*)m")
OTHER_CSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
OSC = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")

# Measured from a live herdr pane with palette_probe.py: this is the palette a
# pane app actually receives, i.e. the host terminal's, not herdr's chrome.
DARK = {
    "name": "dark terminal (measured)",
    "bg": "#212734",
    "fg": "#e6e6e6",
    "ansi": ["#1d1f21", "#cc6666", "#b5bd68", "#f0c674", "#81a2be", "#b294bb",
             "#8abeb7", "#c5c8c6", "#666666", "#d54e53", "#b9ca4a", "#e7c547",
             "#7aa6da", "#c397d8", "#70c0b1", "#eaeaea"],
}

# The published Solarized Light ANSI-16 assignment (Ethan Schoonover's spec).
LIGHT = {
    "name": "light terminal (solarized light)",
    "bg": "#fdf6e3",
    "fg": "#657b83",
    "ansi": ["#eee8d5", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682",
             "#2aa198", "#657b83", "#fdf6e3", "#cb4b16", "#93a1a1", "#839496",
             "#586e75", "#6c71c4", "#073642", "#002b36"],
}

# Every slot collapsed onto one ink: what a genuinely colourless terminal shows.
MONO_DARK = {
    "name": "monochrome (dark)",
    "bg": "#000000",
    "fg": "#d0d0d0",
    "ansi": ["#d0d0d0"] * 16,
}
MONO_LIGHT = {
    "name": "monochrome (light)",
    "bg": "#ffffff",
    "fg": "#1a1a1a",
    "ansi": ["#1a1a1a"] * 16,
}

PALETTES = {"dark": DARK, "light": LIGHT, "mono-dark": MONO_DARK, "mono-light": MONO_LIGHT}


class Cell:
    __slots__ = ("ch", "fg", "bold", "dim", "rev")

    def __init__(self, ch, fg, bold, dim, rev):
        self.ch, self.fg, self.bold, self.dim, self.rev = ch, fg, bold, dim, rev


def parse(stream):
    """Flatten a captured line stream into rows of styled cells."""
    stream = OSC.sub("", stream)
    rows, cur = [], []
    fg, bold, dim, rev = None, False, False, False

    for line in stream.split("\n"):
        line = line.replace("\r", "")
        pos = 0
        while pos < len(line):
            m = SGR.match(line, pos)
            if m:
                params = [int(p or 0) for p in m.group(1).split(";")] or [0]
                i = 0
                while i < len(params):
                    p = params[i]
                    if p == 0:
                        fg, bold, dim, rev = None, False, False, False
                    elif p == 1:
                        bold = True
                    elif p == 2:
                        dim = True
                    elif p == 7:
                        rev = True
                    elif p in (22,):
                        bold = dim = False
                    elif p == 27:
                        rev = False
                    elif 30 <= p <= 37:
                        fg = p - 30
                    elif 90 <= p <= 97:
                        fg = p - 90 + 8
                    elif p == 39:
                        fg = None
                    elif p == 38 and i + 2 < len(params) and params[i + 1] == 5:
                        fg = params[i + 2]
                        i += 2
                    i += 1
                pos = m.end()
                continue
            m = OTHER_CSI.match(line, pos)
            if m:
                pos = m.end()
                continue
            cur.append(Cell(line[pos], fg, bold, dim, rev))
            pos += 1
        rows.append(cur)
        cur = []
    return rows


def resolve(cell, pal):
    fg = pal["fg"] if cell.fg is None else pal["ansi"][cell.fg] if cell.fg < 16 else pal["fg"]
    bg = pal["bg"]
    if cell.rev:
        fg, bg = bg, fg
    return fg, bg


def render(rows, pal, title):
    out = [f'<section><h2>{html.escape(title)} — {html.escape(pal["name"])}</h2>',
           f'<pre style="background:{pal["bg"]}">']
    for row in rows:
        # Coalesce runs of identical style: per-character spans leave subpixel
        # seams that read as banding across a reversed row, which would be a
        # defect of this renderer and not of the board.
        line, run, run_style = [], [], None
        for c in row:
            fg, bg = resolve(c, pal)
            style = f"color:{fg}"
            if bg != pal["bg"]:
                style += f";background:{bg}"
            if c.bold:
                style += ";font-weight:700"
            if c.dim:
                style += ";opacity:.55"
            if style != run_style:
                if run:
                    line.append(f'<span style="{run_style}">{html.escape("".join(run))}</span>')
                run, run_style = [], style
            run.append(c.ch)
        if run:
            line.append(f'<span style="{run_style}">{html.escape("".join(run))}</span>')
        out.append("".join(line) or "&nbsp;")
    out.append("</pre></section>")
    return "\n".join(out)


def main():
    captures = sorted(Path(sys.argv[1]).glob("*.ansi"))
    which = sys.argv[3].split(",") if len(sys.argv) > 3 else list(PALETTES)
    parts = ["""<meta charset="utf-8"><style>
body{font-family:-apple-system,system-ui,sans-serif;margin:0;padding:16px;background:#3a3a3a;color:#eee}
h1{font-size:15px;letter-spacing:.08em;text-transform:uppercase;opacity:.75;margin:0 0 12px}
h2{font-size:12px;font-weight:600;margin:18px 0 4px}
section{overflow:hidden}
pre{font-family:"SF Mono",Menlo,monospace;font-size:11px;line-height:1.3;
    padding:8px;margin:0;border-radius:4px;white-space:pre;display:inline-block;
    transform-origin:top left}
</style><h1>herdr-board render check</h1>"""]
    for cap in captures:
        rows = parse(cap.read_text(errors="replace"))
        # Trim the trailing blank rows the capture pads with.
        while rows and not "".join(c.ch for c in rows[-1]).strip():
            rows.pop()
        for key in which:
            parts.append(render(rows, PALETTES[key], cap.stem))
    Path(sys.argv[2]).write_text("\n".join(parts))
    print(f"{len(captures)} captures x {len(which)} palettes -> {sys.argv[2]}")


main()
