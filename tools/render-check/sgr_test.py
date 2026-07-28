#!/usr/bin/env python3
"""Check the parser the render checks trust.

Everything here reads a capture through sgr.py, so a wrong answer from sgr.py is
a wrong answer everywhere with nothing to catch it. These are the cases that
matter: the run-merging that made escape-sequence counting lie, and the SGR
codes the board actually emits.

    python3 tools/render-check/sgr_test.py
"""
import sys

import sgr

failed = []
ran = 0


def check(name, got, want):
    global ran
    ran += 1
    if got != want:
        failed.append(f"{name}: got {got!r}, want {want!r}")


def cells(grid):
    return [c for row in grid for c in row]


# The defect this file exists for. Two SGR streams that put bold on the same
# four cells: one as a single run, one broken into two by a colour change in the
# middle. Counting sequences says the second has more bold; counting cells says
# they are the same, which is what is true on screen.
one_run = "\x1b[1mABCD\x1b[0m"
split_run = "\x1b[1;31mAB\x1b[1;32mCD\x1b[0m"
check("bold cells, one run", sum(1 for c in cells(sgr.parse(one_run)) if c.bold), 4)
check("bold cells, split run", sum(1 for c in cells(sgr.parse(split_run)) if c.bold), 4)
check("the sequence count is what differs",
      one_run.count("\x1b[") != split_run.count("\x1b["), True)

# Removing hue merges runs without touching emphasis: same cells, fewer runs.
coloured = "\x1b[1;31mAB\x1b[1;32mCD\x1b[0m"
monochrome = "\x1b[1mABCD\x1b[0m"
col, mono = sgr.parse(coloured)[0], sgr.parse(monochrome)[0]
check("emphasis survives the merge", [c.emphasis for c in col], [c.emphasis for c in mono])
check("glyphs survive the merge", [c.ch for c in col], [c.ch for c in mono])
check("hue is what went", [c.hue for c in mono], [None] * 4)

# Attributes the board uses, and the resets that turn each of them off.
row = sgr.parse("\x1b[1mB\x1b[22mn\x1b[2mD\x1b[22mn\x1b[7mR\x1b[27mn"
                "\x1b[3mI\x1b[23mn\x1b[4mU\x1b[24mn")[0]
check("bold then off", [c.bold for c in row], [1, 0, 0, 0, 0, 0, 0, 0, 0, 0])
check("dim then off", [c.dim for c in row], [0, 0, 1, 0, 0, 0, 0, 0, 0, 0])
check("reverse then off", [c.rev for c in row], [0, 0, 0, 0, 1, 0, 0, 0, 0, 0])
check("italic then off", [c.italic for c in row], [0, 0, 0, 0, 0, 0, 1, 0, 0, 0])
check("underline then off", [c.underline for c in row], [0, 0, 0, 0, 0, 0, 0, 0, 1, 0])

# Colour slots: the sixteen the board is allowed, plus the extended forms, which
# have to be consumed whole or their parameters get read as attributes.
check("ansi 8", [c.fg for c in sgr.parse("\x1b[34mx")[0]], [4])
check("ansi bright 8", [c.fg for c in sgr.parse("\x1b[94mx")[0]], [12])
check("default fg", [c.fg for c in sgr.parse("\x1b[34m\x1b[39mx")[0]], [None])
check("background", [c.bg for c in sgr.parse("\x1b[41mx")[0]], [1])
check("bright background", [c.bg for c in sgr.parse("\x1b[101mx")[0]], [9])
check("default bg", [c.bg for c in sgr.parse("\x1b[41m\x1b[49mx")[0]], [None])
check("256-colour", [c.fg for c in sgr.parse("\x1b[38;5;214mx")[0]], [214])
check("truecolour", [c.fg for c in sgr.parse("\x1b[38;2;1;2;3mx")[0]], [("rgb", 1, 2, 3)])
# 38;2;1;2;3 ends in a 3, and a parser that did not consume the selector would
# read that trailing 3 as italic.
check("extended selector consumed whole",
      [c.italic for c in sgr.parse("\x1b[38;2;1;2;3mx")[0]], [False])
check("reset clears everything",
      [(c.fg, c.bg, c.emphasis) for c in sgr.parse("\x1b[1;7;31;42m\x1b[0mx")[0]],
      [(None, None, (False, False, False, False, False))])

# Cursor moves and OSC are stripped, not printed as text.
check("csi is not text", [c.ch for c in sgr.parse("\x1b[2J\x1b[Hx")[0]], ["x"])
check("osc is not text", [c.ch for c in sgr.parse("\x1b]0;title\x07x")[0]], ["x"])

# Trailing pad is not board output — but a reversed space is: it is how the
# selected row reaches the right-hand edge.
check("trailing blanks trimmed", [len(r) for r in sgr.trim(sgr.parse("ab   \n\n"))], [2])
check("a reversed space is kept", len(sgr.trim(sgr.parse("\x1b[7ma  "))[0]), 3)
check("blank rows trimmed off the bottom", len(sgr.trim(sgr.parse("a\n\n\n"))), 1)

# Padding to a rectangle must not alias one cell across many positions, since
# callers edit grids in place.
grid = sgr.rectangle(sgr.parse("abc\na"))
check("padded to a rectangle", [len(r) for r in grid], [3, 3])
grid[1][2].ch = "!"
check("pad cells are not aliased", grid[1][1].ch, " ")

# Reverse video swaps the pair, which is what makes a selected row readable when
# every slot has collapsed onto one ink.
pal = sgr.PALETTES["mono-dark"]
check("reverse swaps fg and bg",
      sgr.resolve(sgr.parse("\x1b[7mx")[0][0], pal), (pal["bg"], pal["fg"]))
check("monochrome collapses the slots",
      sgr.resolve(sgr.parse("\x1b[31mx")[0][0], pal)[0],
      sgr.resolve(sgr.parse("\x1b[32mx")[0][0], pal)[0])

for f in failed:
    print(f"FAIL {f}")
print(f"{len(failed)} of {ran} checks failed" if failed else f"ok: {ran} checks")
sys.exit(1 if failed else 0)
