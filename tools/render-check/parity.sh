#!/bin/bash
# Drive the same action from the keyboard and from the mouse, and diff the
# screen each one leaves behind. Real SGR mouse reports, into a real pane.
#
# Each half of each pair gets a pane of its own. The old shape reset by sending
# `q` and relaunching in the same pane, which is exactly the trap: a pane whose
# `q` was swallowed does not relaunch, it retypes, and the mouse half then
# "matches" the keyboard half because both halves are the same instance. See
# pane.sh.
#
# The comparison is cell by cell — glyph, emphasis and hue at every position —
# rather than a diff of the escape stream, where a difference in run boundaries
# reads as a difference on screen and a difference in trailing pad reads as one
# too. Elapsed counters are normalised out; they are the board's only motion.
#
# Usage: parity.sh <outdir> [parent-pane]
set -u
cd "$(dirname "$0")/../.." || exit 1
. tools/render-check/pane.sh

OUT="${1:?usage: parity.sh <outdir> [parent-pane]}"
RC_PARENT=$(rc_parent "${2:-}")
DEMO="$RC_BOARD demo populated"
mkdir -p "$OUT"
rm -f "$OUT"/*.ansi "$OUT"/*.json 2>/dev/null

fail=0

# A press and its release, as an SGR mouse report, sent as literal text so it
# reaches the app exactly as a terminal would deliver it.
rc_click() {
  herdr pane send-text "$1" \
    "$(printf '\033[<0;%d;%dM\033[<0;%d;%dm' "$2" "$3" "$2" "$3")" >/dev/null
  sleep "$RC_KEY_DELAY"
}
# Both presses in one payload: a double-click has to land inside the 400ms window.
rc_dblclick() {
  herdr pane send-text "$1" \
    "$(printf '\033[<0;%d;%dM\033[<0;%d;%dm\033[<0;%d;%dM\033[<0;%d;%dm' \
       "$2" "$3" "$2" "$3" "$2" "$3" "$2" "$3")" >/dev/null
  sleep 0.5
}

# A fresh pane with the board verified up in it, ready to be pointed at.
rc_mouse_pane() {
  rc_open "$RC_PARENT"
  rc_launch "$RC_PANE" "$DEMO"
  sleep "$RC_SETTLE"
}
rc_mouse_shot() {  # <name>
  sleep "$RC_SETTLE"
  rc_read "$OUT" "$1" "$RC_PANE" "mouse" 0 mouse
  rc_close "$RC_PANE"
}

# Column of a footer hint on this pane, as a 1-based SGR coordinate. Read from
# the pane rather than assumed, because the footer reflows with the width.
rc_click_hint() {  # <pane> <word> <footer-row>
  local hint
  hint=$(herdr pane read "$1" --source visible --format text | tail -1)
  rc_click "$1" "$(awk -v s="$hint" -v w="$2" 'BEGIN{print index(s,w)+1}')" "$3"
}

compare() {
  echo "$1"
  python3 tools/render-check/cells.py "$OUT/$1-key.ansi" "$OUT/$1-mouse.ansi" \
    --label-a keyboard --label-b mouse --strict --elapsed || fail=1
  echo
}

# The footer sits on the last terminal row. Measured on a throwaway pane split
# from the same parent, so it matches the panes the captures happen in.
rc_open "$RC_PARENT"
FOOTER=$(rc_viewport "$RC_PANE")
rc_close "$RC_PANE"
echo "capturing into $OUT, one pane per screen, split from $RC_PARENT"
echo "footer on row $FOOTER"
echo

echo "enter on a task row -> detail"
rc_capture "$OUT" detail-key "$DEMO" -- j j j enter
rc_mouse_pane; rc_dblclick "$RC_PANE" 20 7; rc_mouse_shot detail-mouse
compare detail                                  # same row: app row 6 = SGR row 7

echo "section header folds"
rc_capture "$OUT" fold-key "$DEMO" -- k enter
rc_mouse_pane; rc_click "$RC_PANE" 5 3; rc_mouse_shot fold-mouse
compare fold                                    # up from LIN-131: app row 2 = SGR row 3

echo "footer hint: ? help"
rc_capture "$OUT" help-key "$DEMO" -- :?
rc_mouse_pane; rc_click_hint "$RC_PANE" "? help" "$FOOTER"; rc_mouse_shot help-mouse
compare help

echo "footer hint: o open"
rc_capture "$OUT" open-key "$DEMO" -- o
rc_mouse_pane; rc_click_hint "$RC_PANE" "o open" "$FOOTER"; rc_mouse_shot open-mouse
compare open

echo "footer hint: g go to pane"
rc_capture "$OUT" goto-key "$DEMO" -- g
rc_mouse_pane; rc_click_hint "$RC_PANE" "g go to pane" "$FOOTER"; rc_mouse_shot goto-mouse
compare goto

exit $fail
