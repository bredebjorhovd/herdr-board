#!/usr/bin/env python3
"""Turn captured pane ANSI into a contact sheet I can actually look at.

Reads the raw SGR stream the board emits (`herdr pane read --format ansi`) and
re-renders it under a named 16-colour palette, so the same capture can be
inspected on a dark terminal, a light one, and a colourless one.

The parser lives in sgr.py; this file is only the page. Each capture is
labelled with the provenance capture.sh recorded beside it — pid, argv and
environment — because "which instance drew this" is not answerable by looking.

Usage: ansi_sheet.py <capture-dir> <out.html> [palette,palette,...]
"""
import html
import json
import sys
from pathlib import Path

import sgr


def provenance(cap):
    """The sidecar capture.sh wrote, if there is one."""
    side = cap.with_suffix(".json")
    if not side.exists():
        return None
    return json.loads(side.read_text())


def caption(prov):
    if prov is None:
        return "no provenance sidecar — cannot say which instance drew this"
    env = " ".join(prov.get("env") or []) or "inherited env"
    keys = " ".join(prov.get("keys") or []) or "no keys"
    return f'pid {prov["pid"]} · {prov["cols"]}×{prov["rows"]} · {env} · {keys}'


def render(rows, pal, title, sub):
    out = [f'<section><h2>{html.escape(title)} — {html.escape(pal["name"])}</h2>',
           f'<p class="prov">{html.escape(sub)}</p>',
           f'<pre style="background:{pal["bg"]};color:{pal["fg"]}">']
    for row in rows:
        # Coalesce runs of identical style: per-character spans leave subpixel
        # seams that read as banding across a reversed row, which would be a
        # defect of this renderer and not of the board.
        line, run, run_style = [], [], None
        for c in row:
            fg, bg = sgr.resolve(c, pal)
            style = f"color:{fg}"
            if bg != pal["bg"]:
                style += f";background:{bg}"
            if c.bold:
                style += ";font-weight:700"
            if c.dim:
                style += ";opacity:.55"
            if c.italic:
                style += ";font-style:italic"
            if c.underline:
                style += ";text-decoration:underline"
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
    which = sys.argv[3].split(",") if len(sys.argv) > 3 else list(sgr.PALETTES)
    parts = ["""<meta charset="utf-8"><style>
body{font-family:-apple-system,system-ui,sans-serif;margin:0;padding:16px;background:#3a3a3a;color:#eee}
h1{font-size:15px;letter-spacing:.08em;text-transform:uppercase;opacity:.75;margin:0 0 12px}
h2{font-size:12px;font-weight:600;margin:18px 0 2px}
p.prov{font-family:"SF Mono",Menlo,monospace;font-size:10px;opacity:.5;margin:0 0 4px}
section{overflow:hidden}
pre{font-family:"SF Mono",Menlo,monospace;font-size:11px;line-height:1.3;
    padding:8px;margin:0;border-radius:4px;white-space:pre;display:inline-block;
    transform-origin:top left}
</style><h1>herdr-board render check</h1>"""]
    for cap in captures:
        rows = sgr.trim(sgr.parse(cap.read_text(errors="replace")))
        sub = caption(provenance(cap))
        for key in which:
            parts.append(render(rows, sgr.PALETTES[key], cap.stem, sub))
    Path(sys.argv[2]).write_text("\n".join(parts))
    print(f"{len(captures)} captures x {len(which)} palettes -> {sys.argv[2]}")


if __name__ == "__main__":
    main()
