#!/bin/bash
# Drive the same action from the keyboard and from the mouse, and diff the
# screen each one leaves behind. Real SGR mouse reports, into a real pane.
set -u
PANE="$1"; OUT="$2"; mkdir -p "$OUT"
K() { herdr pane send-keys "$PANE" "$@" >/dev/null; sleep 0.4; }
CLICK() { herdr pane send-text "$PANE" "$(printf '\033[<0;%d;%dM\033[<0;%d;%dm' "$1" "$2" "$1" "$2")" >/dev/null; sleep 0.4; }
# Both presses in one payload: a double-click has to land inside the 400ms window.
DBLCLICK() { herdr pane send-text "$PANE" "$(printf '\033[<0;%d;%dM\033[<0;%d;%dm\033[<0;%d;%dM\033[<0;%d;%dm' "$1" "$2" "$1" "$2" "$1" "$2" "$1" "$2")" >/dev/null; sleep 0.5; }
SNAP() { sleep 0.5; herdr pane read "$PANE" --source visible --format ansi > "$OUT/$1.ansi"; }
RESET() { herdr pane send-keys "$PANE" q >/dev/null; sleep 1
          herdr pane run "$PANE" "./target/debug/herdr-board demo populated" >/dev/null; sleep 2; }
# The footer sits on the last row, and SGR mouse rows are 1-based.
FOOTER=$(herdr pane get "$PANE" | python3 -c \
  'import json,sys; print(json.load(sys.stdin)["result"]["pane"]["scroll"]["viewport_rows"])')
# Column of a footer hint, as a 1-based SGR coordinate.
hint_at() { awk -v s="$1" -v w="$2" 'BEGIN{print index(s,w)+1}'; }

fail=0
# Elapsed counters tick once a second and are the only motion on the board, so
# they are normalised out before comparing.
norm() { python3 -c "
import re,sys
s=open(sys.argv[1],errors='replace').read()
s=re.sub(r'\d+m\d\ds','ELAPSED',s)
# herdr's snapshot ends a line either with a reset or with padding spaces
# depending on what the pane held before; neither is a board behaviour.
s='\n'.join(re.sub(r'(\x1b\[0m|\s)+$','',l) for l in s.replace('\r','').split('\n'))
print(s.strip())" "$1"; }
cmp_pair() { # name
  norm "$OUT/$1-key.ansi" > "$OUT/$1-key.norm"; norm "$OUT/$1-mouse.ansi" > "$OUT/$1-mouse.norm"
  if diff -q "$OUT/$1-key.norm" "$OUT/$1-mouse.norm" >/dev/null; then
    echo "  same   $1"
  else
    echo "  DIFFER $1"; fail=1
    diff "$OUT/$1-key.norm" "$OUT/$1-mouse.norm" | head -4 | cut -c1-100
  fi
}

echo "enter on a task row -> detail"
RESET; K j j j; K enter; SNAP detail-key
RESET; DBLCLICK 20 7; SNAP detail-mouse               # same row: app row 6 = SGR row 7
cmp_pair detail

echo "section header folds"
RESET; K k; K enter; SNAP fold-key                     # up from LIN-131 onto BLOCKED
RESET; CLICK 5 3; SNAP fold-mouse                      # app row 2 = SGR row 3
cmp_pair fold

echo "footer hint: ? help"
RESET; herdr pane send-text "$PANE" '?' >/dev/null; sleep 0.6; SNAP help-key
RESET; HINT=$(herdr pane read "$PANE" --source visible --format text | tail -1)
CLICK "$(hint_at "$HINT" "? help")" "$FOOTER"; SNAP help-mouse
cmp_pair help

echo "footer hint: o open"
RESET; K o; SNAP open-key
RESET; HINT=$(herdr pane read "$PANE" --source visible --format text | tail -1)
CLICK "$(hint_at "$HINT" "o open")" "$FOOTER"; SNAP open-mouse
cmp_pair open

echo "footer hint: g go to pane"
RESET; K g; SNAP goto-key
RESET; HINT=$(herdr pane read "$PANE" --source visible --format text | tail -1)
CLICK "$(hint_at "$HINT" "g go to pane")" "$FOOTER"; SNAP goto-mouse
cmp_pair goto

exit $fail
