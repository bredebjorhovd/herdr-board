#!/bin/bash
# Walk the live demo through every screen, saving the raw ANSI of each.
set -u
PANE="$1"
OUT="$2"
mkdir -p "$OUT"

shot() { sleep 0.6; herdr pane read "$PANE" --source visible --format ansi > "$OUT/$1.ansi"; }
keys() { herdr pane send-keys "$PANE" "$@" >/dev/null; sleep 0.4; }

# Start from a known screen: quit whatever is running, then relaunch.
herdr pane send-keys "$PANE" q >/dev/null; sleep 1
herdr pane run "$PANE" "./target/debug/herdr-board demo populated" >/dev/null; sleep 2

shot 01-list-populated
keys j j j                  # into WORKING
shot 02-list-selected-working
keys l                      # detail
shot 03-detail
keys p                      # prompt
shot 04-prompt
keys esc
keys esc
herdr pane send-text "$PANE" '?' >/dev/null; sleep 0.4   # help
shot 05-help
keys esc
keys x                      # cancel confirmation on a working row
shot 06-confirm-cancel
keys esc
keys j j j j j j j          # down to REVIEW
shot 07-list-selected-review
keys m                      # merge confirmation
shot 08-confirm-merge
keys y                      # the outcome line, with its glyph
shot 08b-merged
keys esc
keys n                      # scenario: empty
shot 09-empty
keys n                      # scenario: linear-down
shot 10-linear-down
keys n                      # scenario: syncd-dead
shot 11-syncd-dead
keys n                      # scenario: stale-binding
shot 12-stale-binding
ls "$OUT"
