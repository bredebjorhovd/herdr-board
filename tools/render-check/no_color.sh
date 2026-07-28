#!/bin/bash
# Does NO_COLOR strip hue and nothing else?
#
# The claim is exact: every hue gone, no cell's emphasis changed, no cell's
# glyph changed. Both halves of it need a tool that does not exist by default.
#
# Two fresh panes, because the two runs differ only in an environment variable
# and `herdr pane run` in a reused pane would have delivered the second command
# to the first instance as keystrokes — leaving a coloured board on screen and
# calling it the NO_COLOR one. cells.py refuses the comparison if the two
# captures share a pid.
#
# Cells, not escape sequences, because ratatui emits one SGR sequence per run of
# same-styled cells: strip the colour and neighbouring runs merge. Counted that
# way bold appeared to fall from 10 to 6 on a screen where nothing changed.
#
# Usage: no_color.sh [outdir] [parent-pane]
set -u
cd "$(dirname "$0")/../.." || exit 1
. tools/render-check/pane.sh

OUT="${1:-/tmp/no-color}"
RC_PARENT=$(rc_parent "${2:-}")
DEMO="$RC_BOARD demo populated"

mkdir -p "$OUT"
rm -f "$OUT"/*.ansi "$OUT"/*.json 2>/dev/null

fail=0
# Screens where hue does real work, and where two states share one hue: blocked
# and failed are both red, so only the glyph separates them without it.
for screen in "list:" "review:j j j j j j j j j j" "detail:j j j l" "dead:n n n"; do
  name="${screen%%:*}"
  keys="${screen#*:}"
  echo "$name"
  # shellcheck disable=SC2086
  rc_capture "$OUT" "$name-colour" "$DEMO" -- $keys
  # shellcheck disable=SC2086
  rc_capture "$OUT" "$name-nocolor" "$DEMO" --env NO_COLOR=1 -- $keys
  python3 tools/render-check/cells.py \
    "$OUT/$name-colour.ansi" "$OUT/$name-nocolor.ansi" \
    --label-a colour --label-b NO_COLOR --mono b || fail=1
  echo
done

exit $fail
