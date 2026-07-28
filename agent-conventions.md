# Board conventions for coding agents

The canonical text every runtime's agent should have in context. `herdr-board
integration install-conventions` writes it into each runtime's global
instruction file between markers, so all of them say the same thing and a
correction here reaches all of them.

Nothing below is runtime-specific. It was Claude-only for the first 24 attempts
purely because it was first written as a Claude Code skill, and nobody ported
it.

<!-- BEGIN herdr-board conventions -->
## The herdr-board task board

One global queue across every workspace: Linear issues and GitHub issues in,
herdr panes running coding agents out. `herdr-board` is on PATH, and `gh` is
authenticated once per user — neither is specific to the agent you are.

**Read it before acting. Never read `board.db` directly** — the schema changes,
the CLI shape does not.

```bash
herdr-board list --state ready  --json   # what can be picked up
herdr-board list --state review --json   # finished, PR waiting on a human
herdr-board list --state blocked --json  # agent stuck on an approval
herdr-board list --json                  # everything, most urgent first
```

**Write the ticket alongside the work, not after it.** `herdr-board new "title"
--label <repo-label>` costs one line and makes the work traceable — reasoning,
branch, PR, review, closure. Several changes landed under no ticket at all is
the thing that has to be reconstructed later. `--dispatch` creates and releases
in one go.

**Releasing work and waiting for it.** `wait` blocks until the work settles, so
an orchestrator does not have to poll or, worse, go quiet until a human prods
it:

```bash
herdr-board dispatch --task linear:AGE-14
herdr-board dispatch --task linear:AGE-15
herdr-board wait --timeout 3600 --json    # returns when the first one settles
```

With no `--task` it watches everything in flight at the moment it is called, and
returns as soon as any of them reaches `review`, `failed` or `done` — the rows
it returns are the ones that settled. Name `--task` (repeatable) to watch
specific work, `--state` to wait for something else (`blocked` to be called back
when an agent needs an answer). It exits non-zero on timeout, and refuses when
nothing is in flight.

Each row: `id`, `identifier`, `title`, `state`, `source`, `url`, `labels`,
`route`, `workspace`, `runtime`, `pane_id`, `pr_url`, `pr_number`, `branch`,
`dispatched_by`, `attempts`, `dispatchable`.

**You may be told instead of having to wait.** If `dispatch` answers `you will be
prompted in <pane> when it settles`, the board will prompt you right here when
that work ends, with the identifier, how it ended and the PR url. Then do not
block on `wait`: carry on, stay available, and act on the message when it
arrives. Without that line nothing will tell you, and rule 7 applies.

States: `blocked` (agent waiting on input) → `working` → `ready` (nothing
running) → `review` (finished or PR open) → `failed` → `done` (issue closed).
Note `done` means the *issue* is closed; herdr's own `done` is this board's
`review`.

1. **Check `dispatchable` first.** False means no route matches and `dispatch`
   will refuse — say so, do not retry. Fixing it means a route in
   `routing.toml`, which is Brede's call, not yours.
2. **Release work** with `herdr-board dispatch --task <id>`. This cuts a git
   worktree, splits a pane in the routed workspace, starts the agent and sends
   the route's prompt. It returns once the agent is up; the daemon takes it from
   there.
3. **Provenance is automatic.** Dispatching from inside your own herdr pane
   records *your* task as the parent, and the board shows `via <parent>` on the
   child. Never pass `--via` unless releasing work on behalf of a task that is
   not you.
4. **One live attempt per task.** A second dispatch fails cleanly rather than
   spawning a second agent. `max_concurrent_per_workspace` also refuses at cap —
   report the refusal, do not cancel someone else's work to make room.
5. **Cancel** with `herdr-board cancel --task <id>`. This ends the *attempt*, not
   the issue: the row returns to `ready` with its history intact. It does not
   notify a parent agent that may be waiting on it — say so if one exists.
6. **Staleness.** `list` reports what the daemon last wrote, up to 30s old. Run
   `herdr-board sync --once` first when freshness matters more than latency.
   `wait` reconciles as it goes, so it does not lag behind that way.
7. **After releasing work, do not fall silent about it.** That leaves the human
   to notice the agent finished and to prompt you. Either the board will prompt
   you when it settles (see above), or `wait` for it, or say plainly that you
   are leaving it running and that nothing will tell you when it is done.
8. **Never dispatch speculatively.** Releasing work starts a real agent in a real
   repo that commits and opens PRs. A human keypress — or an explicit
   instruction — releases tasks. Reading the board is always safe; dispatching is
   not.

**Reviewing a pull request is how you reach the agent that wrote it.** The board
delivers new comments on an open PR back into the pane that produced it — the
agent is still sitting there with the whole task in context, because a task in
`review` keeps its pane. Issue comments, inline comments on the diff, and review
submissions all arrive; `changes requested` is the clearest. So say what is
wrong on the pull request, rather than describing it to a human to relay.

Three things follow from how the loop is kept closed:

- Only an **idle** agent is woken. Feedback left while it is working is
  delivered when it settles, not on top of its turn.
- Each comment is delivered **once**. Editing a comment does not resend it;
  write a new one.
- If its pane is gone — you closed it, or the session ended — nothing is
  delivered and nothing is re-dispatched. The review then waits on the pull
  request for whoever opens it, and `syncd.log` says so once.

**If you were dispatched by the board, commit your work.** The board has no
callback: it decides an attempt is finished by seeing the pane go idle with
either an open PR or commits on the attempt branch. Work left uncommitted in the
worktree when you stop reads as an agent that did nothing, and the row sits in
`working` until a human notices. Commit even when you are not opening a PR.

`herdr-board doctor` explains a board that looks wrong: missing keys, unreachable
repos, routes pointing at workspaces that do not exist. Prefer it to guessing.

Source lives at `~/dev/herdr-board`; `README.md` there covers routing and setup.
<!-- END herdr-board conventions -->
