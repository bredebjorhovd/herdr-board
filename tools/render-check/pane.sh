#!/bin/bash
# Split, run, read, close. One capture per pane, and never a second one.
#
# Reusing a pane cannot be made safe. If the running demo does not quit — a
# confirmation modal that does not honour `q` is enough — then the next
# `herdr pane run` starts no process at all: the command string is delivered to
# the live TUI as keystrokes. `NO_COLOR=1 ./target/debug/herdr-board demo
# populated` contains o, d, r and q, so it drives the old instance around and
# leaves a board on screen. Every "does this look like the board" check passes,
# on the wrong instance, under the wrong environment, with nothing to read as an
# error. That is how a colour leak under NO_COLOR got measured that was really
# just the previous coloured instance still running.
#
# So: a capture gets a pane of its own, created for it and closed after it. The
# pane is empty when we get it, which is what makes the launch checkable — the
# foreground process must stop being the shell and become ours, at a pid that is
# not the shell's. That proves the one thing no screen can: which process drew
# it. The pid is recorded beside the capture, so a later comparison can refuse
# two captures that turn out to be one instance.
#
# Sourced by capture.sh, parity.sh and no_color.sh. Written for the bash that
# ships with macOS, which is 3.2 and cannot expand an empty array under `set -u`.

RC_BOARD=${RC_BOARD:-./target/debug/herdr-board}
RC_KEY_DELAY=${RC_KEY_DELAY:-0.35}
RC_SETTLE=${RC_SETTLE:-0.6}                 # let the TUI finish drawing
RC_LAUNCH_TIMEOUT=${RC_LAUNCH_TIMEOUT:-100} # tenths of a second

rc_panes=()
RC_PANE=""
RC_PID=0
RC_ARGV=""

rc_die() { echo "render-check: $*" >&2; exit 1; }

# Close anything we opened, however we leave. A leaked pane is a pane some
# later run could inherit, which is the failure this file exists to prevent.
rc_cleanup() {
  local p
  for p in ${rc_panes[@]+"${rc_panes[@]}"}; do
    herdr pane close "$p" >/dev/null 2>&1
  done
  rc_panes=()
}
trap rc_cleanup EXIT INT TERM

rc_json() { python3 -c 'import json,sys
d = json.load(sys.stdin)["result"]
for k in sys.argv[1].split("."):
    d = d[k]
print(d)' "$1"; }

# The pane to split from. Its size decides the size of every capture in a run,
# because they all split the same parent and close before the next one opens.
rc_parent() {
  if [ -n "${1:-}" ]; then echo "$1"
  elif [ -n "${HERDR_PANE_ID:-}" ]; then echo "$HERDR_PANE_ID"
  else herdr pane current | rc_json pane.pane_id
  fi
}

# "<shell_pid> <foreground_pid> <foreground argv...>"
rc_process_info() {
  herdr pane process-info --pane "$1" 2>/dev/null | python3 -c 'import json,sys
try:
    d = json.load(sys.stdin)["result"]["process_info"]
except Exception:
    print(0, 0, "")
    sys.exit(0)
fg = (d.get("foreground_processes") or [{}])[0]
print(d.get("shell_pid") or 0, fg.get("pid") or 0, " ".join(fg.get("argv") or []))'
}

# "<pid> <argv...>" of the board, if it is one of the pane's foreground
# processes, else nothing. All of them are searched, not just the first: the
# board shells out to `herdr plugin config-dir` on startup, and that child can
# be what is in front at the moment we look.
rc_board_process() {
  herdr pane process-info --pane "$1" 2>/dev/null | \
  RC_SHELL="$2" RC_WANT="$(basename "$RC_BOARD")" python3 -c 'import json,os,sys
try:
    d = json.load(sys.stdin)["result"]["process_info"]
except Exception:
    sys.exit(0)
shell, want = os.environ["RC_SHELL"], os.environ["RC_WANT"]
for p in d.get("foreground_processes") or []:
    argv = p.get("argv") or []
    if argv and str(p.get("pid")) != shell and os.path.basename(argv[0]) == want:
        print(p["pid"], " ".join(argv))
        break'
}

# Terminal rows, which is what the app sees and what a 1-based SGR mouse row
# counts. Not `pane layout`'s rect height — that includes herdr's own chrome and
# is two rows larger, so clicking the "footer" by it misses the footer.
rc_viewport() {
  herdr pane get "$1" | rc_json pane.scroll.viewport_rows
}

# rc_open <parent> [ENV=VAL ...] — sets RC_PANE.
# Not via stdout: the pane has to be added to rc_panes in *this* shell, or a
# command substitution would take the bookkeeping (and any rc_die) with it into
# a subshell and leak the pane it just opened.
rc_open() {
  local parent="$1"; shift
  local env_args=() e
  for e in "$@"; do
    env_args[${#env_args[@]}]="--env"
    env_args[${#env_args[@]}]="$e"
  done

  local pane
  pane=$(herdr pane split "$parent" --direction right --no-focus \
           --cwd "$PWD" ${env_args[@]+"${env_args[@]}"} | rc_json pane.pane_id) \
    || rc_die "could not split $parent"
  [ -n "$pane" ] || rc_die "split of $parent returned no pane id"
  rc_panes[${#rc_panes[@]}]="$pane"

  # Wait for the shell, so the command is typed at a prompt and not into a pane
  # that has not got one yet.
  local i shell fg rest
  shell=0
  for ((i = 0; i < RC_LAUNCH_TIMEOUT; i++)); do
    read -r shell fg rest < <(rc_process_info "$pane")
    if [ "$shell" != 0 ] && [ "$shell" = "$fg" ]; then break; fi
    sleep 0.1
  done
  [ "$shell" != 0 ] || rc_die "no shell came up in $pane"
  RC_PANE="$pane"
}

rc_close() {
  herdr pane close "$1" >/dev/null 2>&1
  local keep=() p
  for p in ${rc_panes[@]+"${rc_panes[@]}"}; do
    if [ "$p" != "$1" ]; then keep[${#keep[@]}]="$p"; fi
  done
  rc_panes=(${keep[@]+"${keep[@]}"})
}

# rc_launch <pane> <command> — start it, and prove it started. Sets RC_PID/RC_ARGV.
#
# The proof is worth being pedantic about, because the alternative is a capture
# of some other instance that nothing distinguishes by eye. The pane came out of
# rc_open empty, so a board in front of it at a pid that is not the shell's can
# only be the one this call started.
rc_launch() {
  local pane="$1" cmd="$2"
  local shell_pid fg argv
  read -r shell_pid fg argv < <(rc_process_info "$pane")

  herdr pane run "$pane" "$cmd" >/dev/null || rc_die "pane run failed in $pane"

  local i found=""
  for ((i = 0; i < RC_LAUNCH_TIMEOUT; i++)); do
    found=$(rc_board_process "$pane" "$shell_pid")
    if [ -n "$found" ]; then break; fi
    sleep 0.1
  done

  if [ -z "$found" ]; then
    read -r shell_pid fg argv < <(rc_process_info "$pane")
    if [ "$fg" = "$shell_pid" ]; then
      rc_die "nothing launched in $pane: the shell (pid $shell_pid) is still in front"
    fi
    rc_die "$pane is running '$argv' (pid $fg), not $RC_BOARD"
  fi

  RC_PID="${found%% *}"
  RC_ARGV="${found#* }"
}

# rc_keys <pane> <token...> — a token starting with ':' is sent as literal text,
# so the keys the key parser would swallow ('?') stay expressible.
rc_keys() {
  local pane="$1"; shift
  local t
  for t in "$@"; do
    case "$t" in
      :*) herdr pane send-text "$pane" "${t#:}" >/dev/null ;;
      *)  herdr pane send-keys "$pane" "$t" >/dev/null ;;
    esac
    sleep "$RC_KEY_DELAY"
  done
}

# rc_capture <outdir> <name> <cmd> [--env E=V ...] [-- key tokens ...]
#
# The whole cycle: a pane of its own, a launch we verified, the keys, one read,
# the pane gone. Writes <name>.ansi and <name>.json — pid, argv, geometry,
# environment and the keys that got there.
rc_capture() {
  local out="$1" name="$2" cmd="$3"; shift 3
  local envs=() keys=()
  while [ $# -gt 0 ]; do
    case "$1" in
      --env) envs[${#envs[@]}]="$2"; shift 2 ;;
      --) shift; keys=("$@"); break ;;
      *) rc_die "rc_capture: unexpected argument '$1'" ;;
    esac
  done

  mkdir -p "$out"
  rc_open "$RC_PARENT" ${envs[@]+"${envs[@]}"}
  local pane="$RC_PANE"
  rc_launch "$pane" "$cmd"
  sleep "$RC_SETTLE"
  if [ ${#keys[@]} -gt 0 ]; then rc_keys "$pane" "${keys[@]}"; fi
  sleep "$RC_SETTLE"

  rc_read "$out" "$name" "$pane" "$cmd" "${#envs[@]}" \
    ${envs[@]+"${envs[@]}"} ${keys[@]+"${keys[@]}"}
  rc_close "$pane"
}

# rc_read <outdir> <name> <pane> <what> <n-env> [env... keys...]
# One read, and the provenance beside it. Split out because parity.sh drives its
# mouse half by hand and still has to record which instance it captured.
rc_read() {
  local out="$1" name="$2" pane="$3" what="$4" nenv="$5"; shift 5
  herdr pane read "$pane" --source visible --format ansi > "$out/$name.ansi" \
    || rc_die "could not read $pane"

  RC_SIDE_FILE="$out/$name.json" RC_SIDE_ANSI="$out/$name.ansi" \
  RC_SIDE_NAME="$name" RC_SIDE_PANE="$pane" RC_SIDE_CMD="$what" \
  RC_SIDE_VIEWPORT="$(rc_viewport "$pane")" RC_SIDE_NENV="$nenv" \
  RC_PID="$RC_PID" RC_ARGV="$RC_ARGV" RC_DIR="$(dirname "${BASH_SOURCE[0]}")" \
  python3 -c 'import json,os,sys
sys.path.insert(0, os.environ["RC_DIR"])
import sgr
grid = sgr.rectangle(sgr.trim(sgr.parse(
    open(os.environ["RC_SIDE_ANSI"], errors="replace").read())))
n = int(os.environ["RC_SIDE_NENV"])
json.dump({"name": os.environ["RC_SIDE_NAME"],
           "pane_id": os.environ["RC_SIDE_PANE"],
           "pid": int(os.environ["RC_PID"]),
           "argv": os.environ["RC_ARGV"].split(),
           "cmd": os.environ["RC_SIDE_CMD"],
           "cols": max((len(r) for r in grid), default=0),
           "rows": len(grid),
           "viewport_rows": int(os.environ["RC_SIDE_VIEWPORT"]),
           "env": sys.argv[1:1 + n],
           "keys": sys.argv[1 + n:]},
          open(os.environ["RC_SIDE_FILE"], "w"), indent=2)
print("  %s  pid %s  %dx%d  %s" % (
    os.environ["RC_SIDE_NAME"], os.environ["RC_PID"],
    max((len(r) for r in grid), default=0), len(grid),
    " ".join(sys.argv[1:1 + n]) or "inherited env"))' \
    ${@+"$@"} || rc_die "could not write provenance for $name"
}
