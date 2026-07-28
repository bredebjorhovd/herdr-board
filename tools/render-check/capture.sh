#!/bin/bash
# Walk the demo through every screen, saving the raw ANSI of each.
#
# Every screen gets a pane of its own and a key path replayed from launch, so
# nothing a capture does can reach the next one. The old shape — one pane, `q`,
# relaunch, keep going — was wrong twice over: `q` is not honoured by a
# confirmation modal, so the relaunch typed its command into the surviving
# instance; and one stuck `esc` put every screen after it off by one, with no
# way to tell from the captures. See pane.sh.
#
# Usage: capture.sh <outdir> [parent-pane]
set -u
cd "$(dirname "$0")/../.." || exit 1
. tools/render-check/pane.sh

OUT="${1:?usage: capture.sh <outdir> [parent-pane]}"
RC_PARENT=$(rc_parent "${2:-}")
DEMO="$RC_BOARD demo populated"

echo "capturing into $OUT, one pane per screen, split from $RC_PARENT"
rm -f "$OUT"/*.ansi "$OUT"/*.json 2>/dev/null

# name                        command   keys from a fresh launch
rc_capture "$OUT" 01-list-populated       "$DEMO"
rc_capture "$OUT" 02-list-selected-working "$DEMO" -- j j j
rc_capture "$OUT" 03-detail               "$DEMO" -- j j j l
rc_capture "$OUT" 04-prompt               "$DEMO" -- j j j l p
rc_capture "$OUT" 05-help                 "$DEMO" -- :?
rc_capture "$OUT" 06-confirm-cancel       "$DEMO" -- j j j x
rc_capture "$OUT" 07-list-selected-review "$DEMO" -- j j j j j j j j j j
rc_capture "$OUT" 08-confirm-merge        "$DEMO" -- j j j j j j j j j j m
rc_capture "$OUT" 08b-merged              "$DEMO" -- j j j j j j j j j j m y
rc_capture "$OUT" 09-empty                "$DEMO" -- n
rc_capture "$OUT" 10-linear-down          "$DEMO" -- n n
rc_capture "$OUT" 11-syncd-dead           "$DEMO" -- n n n
rc_capture "$OUT" 12-stale-binding        "$DEMO" -- n n n n

echo
ls "$OUT"
