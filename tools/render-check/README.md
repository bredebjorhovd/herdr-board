# Render check

Three items on the design's "verify before you ship" list can only be settled by
looking:

* rendered against several terminal palettes, including a light one
* readable in a monochrome terminal, with no two states reading alike
* keyboard and mouse produce identical behaviour for every action

Tests already assert the *rules* behind the first two — ANSI-16 only, no
background fills, a shape-distinct glyph per state. These scripts produce the
evidence: they drive `herdr-board demo` in a real herdr pane, capture what it
actually emits, and re-render it under palettes you choose.

Everything here needs a running herdr session and a pane to drive. Build first;
the scripts run `./target/debug/herdr-board`.

```bash
cargo build
PANE=$(herdr pane current | python3 -c 'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["pane_id"])')
```

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
bash tools/render-check/capture.sh "$PANE" /tmp/caps
python3 tools/render-check/ansi_sheet.py /tmp/caps /tmp/sheet.html light,mono-dark,dark
```

`capture.sh` walks every screen and scenario — list, detail, prompt, help, both
confirmations and the outcome line, and all five fixtures — saving the raw SGR
stream of each. `ansi_sheet.py` re-renders those captures under named palettes
into one HTML page: a measured dark terminal, published Solarized Light, and
monochrome in both polarities, where all sixteen slots collapse onto one ink.

Open the page. Monochrome is the one to read first: if two states look alike
there, the glyphs are wrong.

## Keyboard and mouse

```bash
bash tools/render-check/parity.sh "$PANE" /tmp/parity
```

Performs each action twice — once from the keyboard, once from a real SGR mouse
report written to the pane's input — and diffs the screen each leaves behind.
Elapsed counters are normalised out; they are the board's only motion. Exits
non-zero on any difference.

The unit tests cover the key map and the hit-testing; this covers the wiring
between them, which is the part that had never been driven.
