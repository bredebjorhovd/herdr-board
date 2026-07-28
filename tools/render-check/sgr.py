#!/usr/bin/env python3
"""Resolve a captured SGR stream into a grid of per-cell attributes.

Everything here that reads a capture goes through this. The unit is the *cell*,
never the escape sequence: ratatui emits one SGR sequence per run of same-styled
cells, so a run count answers a question nobody asked. Remove colour and
neighbouring runs merge — the sequence count falls while every cell keeps the
emphasis it had. Counting sequences showed bold dropping 10 to 6 on a screen
where 53 cells were bold before and 53 after.

Both consumers import from here: `ansi_sheet.py` re-renders a grid under a
palette, `cells.py` compares two grids cell by cell.
"""
import re

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
    """One character with the attributes in force when it was emitted.

    `fg` and `bg` are None for the terminal default, an int for an indexed
    colour, or an ("rgb", r, g, b) tuple. Anything that is not None is a hue the
    board asked for, which is what makes "no hue survived" a checkable claim
    rather than an eyeballed one.
    """

    __slots__ = ("ch", "fg", "bg", "bold", "dim", "italic", "underline", "rev")

    def __init__(self, ch, fg=None, bg=None, bold=False, dim=False,
                 italic=False, underline=False, rev=False):
        self.ch = ch
        self.fg, self.bg = fg, bg
        self.bold, self.dim = bold, dim
        self.italic, self.underline = italic, underline
        self.rev = rev

    @property
    def emphasis(self):
        """Everything that is not hue. NO_COLOR strips hue and leaves this."""
        return (self.bold, self.dim, self.italic, self.underline, self.rev)

    @property
    def hue(self):
        """Both colour slots, or None when the cell asked for neither."""
        return None if self.fg is None and self.bg is None else (self.fg, self.bg)

    @property
    def blank(self):
        """A cell that paints nothing: trailing pad, not board output."""
        return self.ch == " " and not self.rev and self.bg is None


class Pen:
    """The SGR state machine, as a mutable set of attributes."""

    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.rev = False

    def stamp(self, ch):
        return Cell(ch, self.fg, self.bg, self.bold, self.dim,
                    self.italic, self.underline, self.rev)

    def apply(self, params):
        i = 0
        while i < len(params):
            p = params[i]
            if p == 0:
                self.reset()
            elif p == 1:
                self.bold = True
            elif p == 2:
                self.dim = True
            elif p == 3:
                self.italic = True
            elif p == 4:
                self.underline = True
            elif p == 7:
                self.rev = True
            elif p == 22:
                self.bold = self.dim = False
            elif p == 23:
                self.italic = False
            elif p == 24:
                self.underline = False
            elif p == 27:
                self.rev = False
            elif 30 <= p <= 37:
                self.fg = p - 30
            elif p == 39:
                self.fg = None
            elif 40 <= p <= 47:
                self.bg = p - 40
            elif p == 49:
                self.bg = None
            elif 90 <= p <= 97:
                self.fg = p - 90 + 8
            elif 100 <= p <= 107:
                self.bg = p - 100 + 8
            elif p in (38, 48):
                # Extended colour. Consume the whole selector, so a truecolour
                # leak is reported as a hue rather than silently mis-parsed into
                # the parameters that follow it.
                slot = "fg" if p == 38 else "bg"
                if i + 2 < len(params) and params[i + 1] == 5:
                    setattr(self, slot, params[i + 2])
                    i += 2
                elif i + 4 < len(params) and params[i + 1] == 2:
                    setattr(self, slot, ("rgb",) + tuple(params[i + 2:i + 5]))
                    i += 4
            i += 1


def parse(stream):
    """Flatten a captured line stream into rows of styled cells."""
    stream = OSC.sub("", stream)
    pen = Pen()
    rows = []

    for line in stream.split("\n"):
        line = line.replace("\r", "")
        row, pos = [], 0
        while pos < len(line):
            m = SGR.match(line, pos)
            if m:
                pen.apply([int(p or 0) for p in m.group(1).split(";")] or [0])
                pos = m.end()
                continue
            m = OTHER_CSI.match(line, pos)
            if m:
                pos = m.end()
                continue
            row.append(pen.stamp(line[pos]))
            pos += 1
        rows.append(row)
    return rows


def trim(rows):
    """Drop trailing pad — blank cells at the end of a row, blank rows at the
    end of a screen. herdr ends a snapshot line with a reset or with spaces
    depending on what the pane held before; neither is a board behaviour."""
    rows = [list(r) for r in rows]
    for row in rows:
        while row and row[-1].blank:
            row.pop()
    while rows and not rows[-1]:
        rows.pop()
    return rows


def rectangle(rows, width=None):
    """Pad every row to a common width, so cell N of one grid is cell N of the
    other. Comparing ragged rows would silently align the wrong columns.

    The pad cells are fresh rather than a shared blank: callers edit grids in
    place (normalising the elapsed counters is one), and one aliased cell
    standing in for a thousand positions is a bug waiting to be written."""
    width = width if width is not None else max((len(r) for r in rows), default=0)
    return [list(r) + [Cell(" ") for _ in range(width - len(r))] for r in rows]


def resolve(cell, pal):
    """The two colours this cell paints under `pal`, reverse video applied."""
    def slot(colour, default):
        if colour is None:
            return default
        if isinstance(colour, int) and colour < 16:
            return pal["ansi"][colour]
        if isinstance(colour, tuple):
            return "#%02x%02x%02x" % tuple(min(255, c) for c in colour[1:4])
        return default

    fg = slot(cell.fg, pal["fg"])
    bg = slot(cell.bg, pal["bg"])
    if cell.rev:
        fg, bg = bg, fg
    return fg, bg
