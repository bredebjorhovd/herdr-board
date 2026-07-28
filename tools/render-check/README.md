# Render check

Some items on the design's "verify before you ship" list can only be settled by
looking:

* rendered against several terminal palettes, including a light one
* readable in a monochrome terminal, with no two states reading alike
* keyboard and mouse produce identical behaviour for every action

Tests already assert the *rules* behind the first two — ANSI-16 only, no
background fills, a shape-distinct glyph per state. These scripts produce the
evidence: they drive `herdr-board demo` in real herdr panes, capture what it
actually emits, and re-render or compare it.

Everything here needs a running herdr session. Build first; the scripts run
`./target/debug/herdr-board` and split their panes from the one you are in.

```bash
cargo build
bash tools/render-check/capture.sh /tmp/caps
```

## Two ways this lied

Both of these produced a *confident wrong answer* rather than an error, which is
why the tooling is shaped the way it is now.

**A stale TUI satisfies any plausibility guard.** Checking that the pane looks
like the board after a reset is not enough. If the running demo does not quit —
a confirmation modal that does not honour `q` is enough — then the next
`herdr pane run` starts no process at all: its command string is delivered to
the live TUI as *keystrokes*. `NO_COLOR=1 ./target/debug/herdr-board demo
populated` contains `o`, `d`, `r` and `q`, so it drives the old instance around
and leaves a board on screen that passes every "is this the board" check while
being the wrong instance entirely, under the wrong environment. A colour leak
under `NO_COLOR` got measured this way that was really the previous coloured
instance.

Reset-and-reuse cannot be made safe, because the failure is indistinguishable
from success by inspecting the screen. So no pane is ever reused: **split, run,
read, close**, one capture per pane. `pane.sh` does that, and proves the launch
happened — the pane it splits is empty, so a board in front of it at a pid that
is not the shell's can only be the one it just started. That pid is recorded
beside the capture, and `cells.py` refuses to compare two captures that share
one.

**Counting escape sequences measures the wrong thing.** ratatui emits one SGR
sequence per *run* of same-styled cells, so removing colour merges runs and the
sequence count falls even though every cell keeps its emphasis. On the populated
list:

```
list-colour    SGR sequences= 127  bold introducers= 10  BOLD CELLS=53
list-nocolor   SGR sequences= 104  bold introducers=  6  BOLD CELLS=53
```

Counting sequences says bold dropped by four. Counting cells says nothing
changed, which is the truth. So captures are resolved to per-cell attributes and
compared cell by cell — `sgr.py` holds the SGR state machine, and everything
that reads a capture goes through it. A wrong answer there would be a wrong
answer everywhere, so `sgr_test.py` checks it, run-merging first.

## Where the colours come from

```bash
python3 tools/render-check/palette_probe.py /tmp/palette.json
```

Queries OSC 4 and OSC 10/11 from inside the pane. Worth running once, because
the answer is not what the plugin docs suggest: **a pane app receives the host
terminal's ANSI palette, not herdr's theme.** Switching `[theme] name` in
`~/.config/herdr/config.toml` restyles herdr's own chrome and leaves the board's
sixteen colours alone. So the palette that matters for this check is the
terminal's, and that is what `ansi_sheet.py` varies.

## Looking at it

```bash
bash tools/render-check/capture.sh /tmp/caps
python3 tools/render-check/ansi_sheet.py /tmp/caps /tmp/sheet.html light,mono-dark,dark
```

`capture.sh` walks every screen and scenario — list, detail, prompt, help, both
confirmations and the outcome line, and all five fixtures — each in a pane of
its own, with its key path replayed from launch. `ansi_sheet.py` re-renders
those captures under named palettes into one HTML page: a measured dark
terminal, published Solarized Light, and monochrome in both polarities, where
all sixteen slots collapse onto one ink. Each capture is labelled with the pid,
size and environment it came from.

Open the page. Monochrome is the one to read first: if two states look alike
there, the glyphs are wrong.

## Monochrome, measured rather than eyeballed

```bash
bash tools/render-check/no_color.sh /tmp/no-color
```

The claim `NO_COLOR` makes is exact — every hue gone, no cell's emphasis
changed, no cell's glyph changed — so it is checked exactly, on four screens
including the two that share red. Two fresh panes per screen, differing only in
the environment `herdr pane split --env` gives them:

```
  grid       90x51 vs 90x51
  colour     1272/4590 painted  bold=53  dim=955  reverse=90  fg=[1, 3, 6]  bg=[]
  NO_COLOR   1272/4590 painted  bold=53  dim=955  reverse=90  fg=[]  bg=[]
  glyph differs: 0 cells   emphasis differs: 0 cells   hue differs: 19 cells
  ok
```

Exits non-zero if a glyph or an emphasis moved, or if any hue survived.

## Keyboard and mouse

```bash
bash tools/render-check/parity.sh /tmp/parity
```

Performs each action twice — once from the keyboard, once from a real SGR mouse
report written to the pane's input — and compares the screen each leaves behind,
cell by cell, hue included. Elapsed counters are normalised out; they are the
board's only motion. Exits non-zero on any difference.

The unit tests cover the key map and the hit-testing; this covers the wiring
between them, which is the part that had never been driven.

## The pieces

| file | |
| --- | --- |
| `pane.sh` | split, run, verify, read, close — sourced by the three scripts |
| `sgr.py` | SGR state machine; capture → grid of per-cell attributes, plus the palettes |
| `sgr_test.py` | 30 checks on that parser — `python3 tools/render-check/sgr_test.py` |
| `cells.py` | compare two captures cell by cell, or census one |
| `ansi_sheet.py` | re-render captures under palettes as an HTML contact sheet |
| `capture.sh` | every screen, one pane each |
| `no_color.sh` | hue gone, emphasis and glyphs untouched |
| `parity.sh` | keyboard against mouse |
| `palette_probe.py` | ask the terminal what its sixteen colours actually are |

`cells.py` is usable on its own:

```bash
python3 tools/render-check/cells.py /tmp/caps/01-list-populated.ansi   # census one
python3 tools/render-check/cells.py a.ansi b.ansi --strict --elapsed   # compare two
```
