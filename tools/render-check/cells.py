#!/usr/bin/env python3
"""Compare two captures cell by cell, and census the attributes of one.

The question a render check actually asks is "what does each cell carry", and
the answer is not recoverable from the escape stream by counting. ratatui emits
one SGR sequence per run of same-styled cells, so anything that merges runs —
removing colour is the obvious one — changes the sequence count without
changing a single cell. Measuring NO_COLOR that way reported bold falling from
10 to 6; measuring cells reported 53 and 53, nothing changed.

    cells.py <capture.ansi>                  census one capture
    cells.py <a.ansi> <b.ansi> [options]     compare two

Options:
    --label-a T --label-b T   names for the report
    --mono a|b                that side must carry no hue at all
    --strict                  hue must match too (for keyboard/mouse parity)
    --elapsed                 blank out the elapsed counters before comparing

Exits non-zero on any required difference. Glyph and emphasis must always
match; hue is reported but only asserted under --strict or --mono.
"""
import json
import re
import sys
from pathlib import Path

import sgr

# The elapsed counters tick once a second and are the board's only motion.
ELAPSED = re.compile(r"\d+m\d\ds")


def load(path):
    grid = sgr.rectangle(sgr.trim(sgr.parse(Path(path).read_text(errors="replace"))))
    side = Path(path).with_suffix(".json")
    prov = json.loads(side.read_text()) if side.exists() else None
    return grid, prov


def blank_elapsed(grid):
    """Replace the characters of every elapsed counter with a sentinel, in
    place, leaving their attributes alone — the counter's styling is board
    output and still worth comparing."""
    for row in grid:
        text = "".join(c.ch for c in row)
        for m in ELAPSED.finditer(text):
            for i in range(m.start(), m.end()):
                row[i].ch = "~"
    return grid


def census(grid):
    flat = [c for row in grid for c in row]
    return {
        "cells": len(flat),
        "painted": sum(1 for c in flat if not c.blank),
        "bold": sum(1 for c in flat if c.bold),
        "dim": sum(1 for c in flat if c.dim),
        "italic": sum(1 for c in flat if c.italic),
        "underline": sum(1 for c in flat if c.underline),
        "reverse": sum(1 for c in flat if c.rev),
        "hued": sum(1 for c in flat if c.hue is not None),
        "fg": sorted({c.fg for c in flat if c.fg is not None}, key=repr),
        "bg": sorted({c.bg for c in flat if c.bg is not None}, key=repr),
    }


def shape(grid):
    return (len(grid), max((len(r) for r in grid), default=0))


def line(label, grid):
    c = census(grid)
    emph = "  ".join(f"{k}={c[k]}" for k in
                     ("bold", "dim", "italic", "underline", "reverse") if c[k])
    return (f"  {label:<10} {c['painted']}/{c['cells']} painted  {emph or 'no emphasis'}"
            f"  fg={c['fg']}  bg={c['bg']}")


def check_provenance(pa, pb, fail):
    """Two captures that came from one process are one capture. That is the
    whole stale-pane failure: `herdr pane run` against a live TUI delivers its
    command string as keystrokes, so the old instance keeps drawing under the
    old environment and the screen still looks like the board."""
    if pa is None or pb is None:
        print("  note: no provenance sidecar; cannot prove these are two instances")
        return
    if pa["pid"] == pb["pid"]:
        fail.append(f"both captures came from pid {pa['pid']} — one of them is stale")
    if (pa["cols"], pa["rows"]) != (pb["cols"], pb["rows"]):
        fail.append(f"pane geometry differs: {pa['cols']}x{pa['rows']} vs "
                    f"{pb['cols']}x{pb['rows']}")


def compare(a_path, b_path, label_a, label_b, mono, strict, elapsed):
    ga, pa = load(a_path)
    gb, pb = load(b_path)
    if elapsed:
        blank_elapsed(ga)
        blank_elapsed(gb)

    fail = []
    check_provenance(pa, pb, fail)

    print(f"  grid       {shape(ga)[1]}x{shape(ga)[0]} vs {shape(gb)[1]}x{shape(gb)[0]}")
    print(line(label_a, ga))
    print(line(label_b, gb))

    if shape(ga) != shape(gb):
        fail.append("grids are different shapes; nothing below is meaningful")
        report(fail)
        return 1

    glyph = emph = hue = 0
    first = {}
    for y, (ra, rb) in enumerate(zip(ga, gb)):
        for x, (ca, cb) in enumerate(zip(ra, rb)):
            if ca.ch != cb.ch:
                glyph += 1
                first.setdefault("glyph", (y, x, repr(ca.ch), repr(cb.ch)))
            if ca.emphasis != cb.emphasis:
                emph += 1
                first.setdefault("emphasis", (y, x, ca.emphasis, cb.emphasis))
            if ca.hue != cb.hue:
                hue += 1
                first.setdefault("hue", (y, x, ca.hue, cb.hue))

    print(f"  glyph differs: {glyph} cells   emphasis differs: {emph} cells"
          f"   hue differs: {hue} cells")
    for kind, (y, x, va, vb) in sorted(first.items()):
        print(f"    first {kind} at row {y} col {x}: {va} vs {vb}")

    if glyph:
        fail.append(f"{glyph} cells changed glyph")
    if emph:
        fail.append(f"{emph} cells changed emphasis")
    if strict and hue:
        fail.append(f"{hue} cells changed hue")

    if mono:
        grid, label = (ga, label_a) if mono == "a" else (gb, label_b)
        hued = census(grid)["hued"]
        if hued:
            fail.append(f"{label} still carries hue on {hued} cells")

    report(fail)
    return 1 if fail else 0


def report(fail):
    for f in fail:
        print(f"  FAIL {f}")
    if not fail:
        print("  ok")


TAKES_VALUE = {"--label-a": "label_a", "--label-b": "label_b", "--mono": "mono"}
BARE = {"--strict": "strict", "--elapsed": "elapsed"}


def parse_args(argv):
    opts = {"label_a": "a", "label_b": "b", "mono": None,
            "strict": False, "elapsed": False}
    paths, i = [], 0
    while i < len(argv):
        arg = argv[i]
        if arg in TAKES_VALUE:
            if i + 1 >= len(argv):
                raise SystemExit(f"{arg} needs a value")
            opts[TAKES_VALUE[arg]] = argv[i + 1]
            i += 2
        elif arg in BARE:
            opts[BARE[arg]] = True
            i += 1
        elif arg.startswith("--"):
            raise SystemExit(f"unknown option {arg}")
        else:
            paths.append(arg)
            i += 1
    if opts["mono"] not in (None, "a", "b"):
        raise SystemExit("--mono takes a or b")
    return paths, opts


def main():
    paths, opts = parse_args(sys.argv[1:])

    if len(paths) == 1:
        grid, prov = load(paths[0])
        print(f"  grid       {shape(grid)[1]}x{shape(grid)[0]}")
        if prov:
            print(f"  from       pid {prov['pid']} {' '.join(prov['argv'])}"
                  f"  env={' '.join(prov.get('env') or []) or 'inherited'}")
        print(line(Path(paths[0]).stem, grid))
        return 0
    if len(paths) != 2:
        print(__doc__)
        return 2

    return compare(paths[0], paths[1], opts["label_a"], opts["label_b"],
                   opts["mono"], opts["strict"], opts["elapsed"])


if __name__ == "__main__":
    sys.exit(main())
