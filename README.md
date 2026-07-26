# herdr-board

A personal task board that runs inside [herdr](https://herdr.dev). Linear issues
(read-write) and GitHub issues/PRs (read-only) come in; one keypress sends a task
into a herdr pane with a coding agent; pane state reconciles back to the board
and to Linear.

Single operator, local-first, no cloud components. Target: herdr ≥ 0.7.5.

```
 herdr-board                                       linear ✓   gh ✓   synced 12s
 ──────────────────────────────────────────────────────────────────────────────
 ▲ BLOCKED  ───────────────────────────────────────────────────────────────────
 ▲ LIN-131   Signicat callback drops the state …  claude-code ws:offhand  6m52s
 ● WORKING  ───────────────────────────────────────────────────────────────────
 ● LIN-138   Rewrite the Tripletex sync cursor    claude-code ws:offhand  9m04s
 ● LIN-140   Backfill missing orgnr on le…  claude-code ws:offhand  idle 11m06s
 ▸ READY  ─────────────────────────────────────────────────────────────────────
 ▸ LIN-145   Add retry to Altinn poller                     [enter to dispatch]
 ▸ LIN-151   Tidy the changelog script                                 no route
 ✓ REVIEW  ────────────────────────────────────────────────────────────────────
 ✓ LIN-129   Split the MVA report by term         PR #291 open · waiting on you
 ✕ FAILED  ────────────────────────────────────────────────────────────────────
 ✕ LIN-122   Migrate the Maskinporten client id  pane exited without completing
 · DONE today  enter to expand  ───────────────────────────────────────────────
 ──────────────────────────────────────────────────────────────────────────────
 enter dispatch · o open in browser · s sync · ? help
```

## Setup

### 1. Build and link

```bash
git clone <this repo> ~/dev/herdr-board
cd ~/dev/herdr-board
cargo build --release
herdr plugin link "$PWD"
```

`plugin link` is the right command while developing locally; it does not run
build commands, so build first. Installation is global to your user and
available in every herdr session.

### 2. Add credentials

Both keys live in one file. **Append** to it rather than overwriting, or you
will drop whichever key is already there:

```bash
CFG="$(herdr plugin config-dir board)"
cat >> "$CFG/.env" <<'EOF'
LINEAR_API_KEY=lin_api_...
# Needed as soon as any repo is listed under [github]. A private repo answers
# 404 rather than 401 without it, so the symptom is not obviously an auth
# problem — `doctor` checks each repo and says so.
GITHUB_TOKEN=ghp_...
EOF
chmod 600 "$CFG/.env"
```

If you already use the `gh` CLI, its token works and needs no new secret:

```bash
{ printf 'GITHUB_TOKEN='; gh auth token; } >> "$CFG/.env"
```

Note it is tied to your `gh` login, so `gh auth logout` breaks the board; a
dedicated fine-grained PAT is steadier if this sticks.

The board pane and `syncd` read this file directly on every reload cycle, so
adding, editing, or removing a key takes effect without a restart. A key
exported in the shell still takes precedence over the file.

The GitHub token needs `repo` scope: the board comments on dispatch and outcome
and closes issues on done. Set `[github] writeback = false` if you would rather
it only read, in which case read scope is enough.

Create the Linear key at **Settings → Security & access → Personal API keys**.
It is sent as a bare `Authorization` header (personal keys do not take `Bearer`).

### 3. Configure routing

```bash
herdr-board init          # generates routing.toml from your herdr workspaces
$EDITOR "$CFG/routing.toml"
```

`init` walks the workspaces herdr already knows about, reads each one's git
remote, and writes a route per GitHub repo — so you do not have to hand-write
repo lists to see anything. Linked worktrees are skipped: those are attempts the
board creates, not projects to route to. It will not overwrite an existing
`routing.toml` without `--force`.

If `LINEAR_API_KEY` is already set it also lists your Linear teams and writes a
route per team, matching each to a herdr workspace by name or key. A team it
cannot match is written commented-out with `CHANGE-ME` rather than guessed at,
and reported on stdout. So the useful order is: key first, then `init`.

Or start from the fuller example instead:

```bash
cp routing.example.toml "$CFG/routing.toml"
```

A route maps issues to a herdr workspace, a repo, and a runtime. First matching
route wins; all keys inside one `match` must match. `workspace` is a herdr
workspace **label** — check yours with `herdr workspace list`.

A task with no matching route still appears on the board, marked `no route`, and
`enter` refuses it rather than guessing.

### 4. Check it

```bash
./target/release/herdr-board doctor
```

`doctor` validates each route against reality: that the herdr workspace exists,
that the repo is a git repo, and that the runtime maps to a herdr agent kind. It
exits non-zero if anything fails.

### 5. Bind the toggle

The board is **global** — one queue across every workspace — but it opens as a
**split beside whatever you are looking at**, not as a tab and not as an overlay.
A tab pins a global queue inside one workspace's layout; an overlay zooms over
your work so you cannot read both at once and have to close it to get back. A
split is an ordinary herdr pane: prefix navigation moves between it and your
work, and closing it restores the layout.

Add to `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+shift+b"
type = "plugin_action"
command = "board.toggle"
description = "board"
```

Then `prefix+shift+b` opens the board beside you, and the same again closes it
and restores the layout. `prefix` is herdr's leader key — `ctrl+b` by default —
so that is `ctrl+b`, release, then `shift+b`.

**Not `prefix+b`**: herdr binds that to `toggle_sidebar`, and the built-in wins.
Check `herdr --default-config` before choosing a key; as of 0.7.5 the free
single letters in prefix mode are roughly `a d f i m t u y`, and `shift+b` keeps
the mnemonic. The board never binds `ctrl+b` or any other
control chord, so herdr's command layer keeps working while the board has focus.

Without a binding, the same thing by hand:

```bash
herdr-board toggle
```

Restart herdr (or start a new session) so the startup hook launches the sync
daemon — or start it yourself with `herdr-board syncd --ensure`.

> A control in herdr's own sidebar, next to `spaces` and `agents`, is not
> available to a plugin: native non-terminal plugin UI is explicitly out of
> scope for plugin v1, and the sidebar is herdr's own chrome. A key-bound split
> is the closest herdr offers.

## Agent-initiated dispatch

**Any agent can dispatch from the board, and this is a primary path — not an
escape hatch.** An agent working in a herdr pane runs:

```bash
herdr-board dispatch --task linear:LIN-145
```

and the row lands on the board within a tick. No flag is needed to say who did
it: the command inherits `HERDR_PANE_ID` from the pane it ran in, and if a live
attempt owns that pane, the agent in it is recorded as the parent. The board pane
and the picker popup own no attempt — a popup gets no pane id at all — so an
operator dispatch resolves to "you" on its own. Pass `--via <task-id>` to state
it explicitly.

Because rows now appear in `working` that you never released, routinely, the
board says who released them:

- **List, from 80 columns**: `via LIN-138` takes the **runtime column** on
  agent-dispatched rows. When an agent chose the runtime, which one it picked is
  the least interesting fact on the row and the parent is the most — so this
  costs zero title width, and the runtime is still in detail.
- **List, from 100 columns**: both — runtime, workspace, `via LIN-138`, elapsed.
- **Detail**, always: a `dispatched by` row — dim `you`, or default fg
  `LIN-138  ·  agent, not you`, because that is the case worth noticing.
- **Prompt view**, always: appended to the provenance line —
  `as sent, attempt 1 · codex · dispatched by LIN-138`.
- **Upstream**: the dispatch comment names the parent too, so reading the Linear
  issue tells you an agent released it.

Provenance names the parent **task**, not the pane: the pane is transient, the
task is what you can navigate to.

**Deliberately not built:** any tree or indent showing dispatch chains. Children
and parents routinely sit in different sections — a child in `ready` under a
parent in `working` — and the fixed section order is the spine of the design.
If chains need to be legible as chains, that is a separate view.

### Cancelling a child, and what the parent is told

Nothing is pushed to the parent. **The board is the channel** — the same one it
already learns everything through.

The question this settles is whether the herd gains parent/child signalling.
It does not. A parent agent already discovers completion, failure and open pull
requests exactly one way: by polling `herdr-board list --json`. Cancellation was
not missing a channel, it was invisible *on* the existing one — `cancelled`
derives back to `ready`, deliberately, because the issue is still owed, and a
`ready` row looked identical whether the operator had killed it or it had never
been dispatched. So the fix is a field, not a protocol:

```json
{ "identifier": "LIN-145", "state": "ready",
  "last_outcome": "cancelled", "last_outcome_at": "2026-07-26T20:41:07Z",
  "dispatched_by": "linear:LIN-138" }
```

A parent polling its child sees `ready` + `cancelled` and knows to stop waiting.

**The contract for a parent agent:** if you dispatch, you poll. Do not block
indefinitely on a child — it can be cancelled out from under you at any moment
by an operator who owes you nothing. Poll `list --json`, and treat
`last_outcome: cancelled` on a row you released as the end of that line of work.

**Pushing a prompt into the parent's pane was rejected.** herdr can do it —
`agent prompt` is right there, and dispatch already uses it — but delivery into a
running agent is unreliable by construction (see `deliver_prompt`, which exists
entirely to fight this: agents swallow pastes mid-turn, and the only evidence of
arrival is the screen changing). Beyond that, `dispatched_by` names a *task*, and
the parent's own pane is frequently gone by the time you cancel; and one operator
keystroke turning into a second agent talking unprompted is action at a distance
in a tool whose whole premise is that you can see what is running.

**What the operator is told instead.** Cancelling is the one board action with a
consequence off the board, and the operator is the only one who can act on it —
so `x` on an agent-dispatched row names the parent rather than repeating that the
issue is still open:

```
✓ cancelled LIN-145 — released by LIN-138, which may be waiting on it
```

`herdr-board cancel` prints the same, followed by `— not notified`. A parent
whose own attempt has already ended is still named, without the claim that it is
waiting: telling you to go poke a finished agent would be worse than silence.

## Using it

| key | context | effect |
|---|---|---|
| `j` `k` `↓` `↑` | list | move selection |
| `enter` | ready row | open the dispatch picker |
| `enter` | other row | detail view |
| `enter` | collapsed done | expand the section |
| `l` | list | detail view |
| `p` | any row | prompt view; again to toggle back |
| `h` `esc` | detail, prompt, help | back |
| `g` | bound row | focus the herdr pane running that task |
| `o` | any row | open the issue (or the PR) in a browser |
| `r` | failed row | re-dispatch as a new attempt |
| `d` | failed row | mark done on the board |
| `m` | review with a PR | merge the pull request — confirms first |
| `x` | working, blocked | cancel — confirms, then kills the pane |
| `s` | anywhere | sync now |
| `?` | anywhere | help |

Mouse works everywhere: click selects, double-click is `enter`, and every footer
hint is a click target.

`ctrl+b` is never claimed. The board ignores every control chord, so the herdr
prefix always reaches herdr.

### What the states mean

| state | glyph | meaning |
|---|---|---|
| blocked | `▲` | the agent is waiting on an approval or a question |
| working | `●` | an agent has the task |
| ready | `▸` | nothing running; `enter` dispatches |
| review | `✓` | finished, or a PR is open — waiting on you. `m` merges it |
| failed | `✕` | the pane exited without completing |
| done | `·` | the issue is closed |

`done` here means the *issue* is closed. herdr's own `done` — an agent finished
and you have not looked yet — is this board's `review`. Same word, different
scope; the difference is deliberate.

State is **derived** on every read from upstream state plus the live attempt, so
it cannot drift. Cancelling ends the attempt, not the issue: the row returns to
`ready` with its attempt history intact. A row whose issue was *deleted* upstream
also lands in `done` — see "Tasks that vanish upstream" — and says `gone
upstream` where the others show a workspace, so the two reasons for being there
stay distinguishable.

## How it fits together

```
syncd (daemon) ──poll──> Linear / GitHub
      │
      ├──reconcile──> herdr pane + agent state
      │
      └──writes──> board.db  <──reads── board pane (TUI)
                       ↑                      │
                       └────── picker ────────┘
```

`board.db` (SQLite, WAL) is the only bus between our processes — no sockets of
our own. The TUI re-reads on a 1 s tick and whenever the file changes.

The plugin and its data are global: one database, every task across every
workspace, linked once per user. Only the overlay is summoned into whichever
workspace you are in at the time. Routing maps tasks *to* workspaces; the board
always shows all of them.

Files, all under the herdr-managed plugin directories:

| path | what |
|---|---|
| `$CONFIG/.env` | `LINEAR_API_KEY`, optional `GITHUB_TOKEN` |
| `$CONFIG/routing.toml` | routes, sync interval, defaults |
| `$STATE/board.db` | tasks, attempts, writeback queue |
| `$STATE/syncd.pid` | daemon pidfile (pid + start time) |
| `$STATE/syncd.log` | every state transition and every herdr argv, rotated at 5 MB |
| `$STATE/wt/` | worktrees cut by older versions; herdr owns new ones. Cleared by `gc`, never automatically |

Nothing is ever written to `HERDR_PLUGIN_ROOT`.

### Routing Linear tickets to repos

Routes match on four keys, ANDed within one `match`, first matching route wins:

| key | matches |
|---|---|
| `linear_team` | team key, e.g. `AGE` |
| `linear_project` | Linear project name |
| `label` | any label on the issue |
| `gh_repo` | `owner/repo`, for GitHub issues |

With one Linear team and several repos, the team says nothing about which
codebase a ticket belongs to — labels are the usual discriminator. Add a label
per repo in Linear and route on it, keeping a team-wide catch-all **last**:

```toml
[[route]]
match = { label = "tally" }
workspace = "tally"
repo = "~/dev/tally"
runtime = "claude-code"

# last: anything in the team with no repo label
[[route]]
match = { linear_team = "AGE" }
workspace = "herdr-board"
repo = "~/dev/herdr-board"
runtime = "claude-code"
```

A catch-all that is not last shadows every route after it, and `doctor` refuses
that config rather than letting it silently mis-route.

Two different things are called labels, which is easy to trip on:

- `[sync] labels` — the **filter**: which issues reach the board at all
  (assigned to you **OR** carrying one of these).
- `[[route]] match = { label = ... }` — the **router**: where a task goes once
  it is on the board.

### Subcommands

| command | role |
|---|---|
| `init` | write a starter routing.toml from your herdr workspaces |
| `toggle` | open the board beside you, or close it if it is up (bind this to a key) |
| `pane` | the board TUI itself (herdr launches this; you do not run it) |
| `picker` | the dispatch popup |
| `syncd [--ensure]` | the daemon; `--ensure` starts it if absent and exits |
| `sync --once` | one sync cycle |
| `list [--state S] [--source S] [--json]` | read the board (for agents) |
| `dispatch --task <id>` | dispatch without the picker |
| `cancel --task <id>` | end the live attempt, keep the issue open |
| `gc [--older-than 14d] [--dry-run]` | remove the worktrees of finished attempts |
| `doctor` | check the environment |
| `demo [scenario]` | render the TUI on fixtures (`--list` for the scenarios) |

### Debugging

`syncd.log` is the story. It records every state transition and the exact argv of
every herdr call, so a misbehaving dispatch can be replayed by hand:

```bash
tail -f ~/.local/state/herdr/plugins/board/syncd.log
```

The header tells you which layer is unhappy:

- `linear ✗ retrying 30s` — the source is down. Rows stay on screen; the
  writeback queue drains when it returns. Backoff caps at 5 minutes.
- `syncd not running   sync stale 4m` — the **daemon** is dead, which is not the
  same thing. The sources may be fine and nobody is asking them. Run
  `herdr-board syncd --ensure`.

## Driving the board from an agent

An orchestrator has a complete loop: read the board, release work, follow it,
cancel it.

```bash
herdr-board list --state ready --json     # what can be picked up
herdr-board list --state review --json    # finished work with PRs waiting
herdr-board dispatch --task gh:owner/repo#87
herdr-board list --json                   # poll: how is my child doing?
herdr-board cancel   --task gh:owner/repo#87
```

The fourth line is not optional. Polling is the **only** way work you released
reports back — there is no callback, no signal, and no message. A child that
finished, failed, or was cancelled by the operator looks like nothing at all
until you look.

`list --json` returns one object per row: `id`, `identifier`, `title`, `state`,
`source`, `url`, `labels`, `route`, `workspace`, `runtime`, `pane_id`, `pr_url`,
`pr_number`, `branch`, `dispatched_by`, `last_outcome`, `last_outcome_at`,
`attempts`, `dispatchable`, and `gone`.
`dispatchable` is false when no route matches *or* the row is `gone`, in which
case `dispatch` will refuse it; `gone` tells the two apart, since only one of
them is fixed by editing `routing.toml`. Rows come back in board order, so the
most urgent are first. Read this rather than `board.db` directly: the schema is
ours to change, this shape is not.

`last_outcome` is how the most recent *ended* attempt ended — `done`, `failed`,
`cancelled` or `orphaned` — with `last_outcome_at` saying when. It stays set
while a newer attempt is live, so a retry does not erase how the previous one
went. This is what makes a cancelled child legible: `state` alone cannot
distinguish a row the operator killed from one that was never dispatched, since
both are `ready`. See "Cancelling a child, and what the parent is told".

`dispatch` from inside an agent's own pane records that agent as the parent
automatically (see above), so a chain of work stays attributable without anyone
passing ids around. **If you dispatch, you poll** — nothing notifies you, and a
child can be cancelled out from under you at any time.

`list` reads whatever the daemon last wrote. Force a refresh first with
`herdr-board sync --once` if freshness matters more than latency.

## Arriving from a link

Ctrl-clicking a Linear or GitHub issue URL in any herdr pane routes to the board
instead of the browser. It lands on the **list with that row selected** — detail
would hide the queue you came to see — expanding the `done` section if the row is
in there. If the board is already open it is focused rather than opened a second
time; if the issue has not been polled yet the handler syncs once and tries
again, and says plainly when the URL matches nothing on the board.

## Claude Code state detection

herdr classifies agent state by matching rules against the bottom of the pane.
Its stock Claude Code manifest has one general `working` rule —
`osc_title_working`, matching a braille spinner in the *terminal title* — and
Claude Code emits no title in a herdr pane, so it can never fire. Meanwhile
`live_prompt_box` (priority 950) matches the prompt box, which Claude Code keeps
on screen while working and beneath its own approval dialogs.

So a thinking or blocked Claude agent reports `idle`, and the board's `blocked`
section — the one state that exists to say "an agent needs you" — never fires
for one. Codex is detected correctly; this is Claude-specific.

```bash
herdr-board integration install claude
```

That writes a local agent-detection override — herdr's supported extension
point, and local overrides always win — adding a rule that matches Claude Code's
on-screen working line, whose token counter only appears mid-turn. It sits above
`live_prompt_box` and below the blocked rules, so an approval prompt still wins.

Two things it is not:

- **Not hooks.** herdr accepts `pane report-agent` from anyone, but Claude
  Code's state authority is the screen manifest and its integrations are
  "intentionally not lifecycle authorities" — reports are accepted and ignored.
- **Not `herdr integration install claude`.** That installs one `SessionStart`
  hook calling `pane.report_agent_session`, which records session identity and
  explicitly not lifecycle state.

An override replaces the manifest wholesale, so it snapshots the active one and
will shadow later upstream updates. `herdr-board integration uninstall claude`
puts it back.

## Notes on the herdr integration

Everything herdr-facing goes through `src/herdr.rs`, which logs every argv. It is
the only file to touch if a herdr verb ever differs from expectation. Verified
against herdr 0.7.5 (`herdr completion zsh` plus the published docs):

- **`runtime` is not a herdr agent kind.** `routing.toml` says `claude-code`
  because that is what the board displays, but herdr's kind is `claude`. The
  mapping lives in `config::herdr_kind_for_runtime`, and `doctor` rejects a
  runtime with no mapping rather than failing at dispatch time.
- **`agent start` cannot create a pane.** It requires an existing pane already at
  its shell prompt, so dispatch does `tab create` first and starts the agent in
  the returned `root_pane`. Each attempt gets its own tab, created with
  `--no-focus` so dispatching does not yank you out of the board.
- **`pane focus` is directional** (`--direction left|right|…`) and cannot target
  a pane id. `g` uses `agent focus <pane_id>`, falling back to `tab focus`.
- **The picker popup is not a herdr pane.** It has no pane id and is outside
  every pane and agent API, so it cannot reconcile anything. It performs the
  dispatch and exits; the board picks the result up from the database.
- **Popup size is declared in the manifest, not on the command line.** herdr
  0.7.5's shell completion does not offer `popup` as a `--placement` value even
  though the docs list it, so `plugin pane open` is called without `--placement`
  and the manifest decides. `width`/`height` are outer cells; 62×16 gives the
  60×14 interior the design specifies.
- **Panes overflow to a new tab.** Right, then down, then a tab of its own:
  splitting without a limit ends in a tab of unreadable slivers, and past three
  panes a new tab is the better trade — it costs a keystroke to reach, but what
  is on screen stays legible. The tab is labelled with the task identifier, so
  the tab bar says which agent is where. `[defaults] max_panes_per_tab`.
- **Splits pick their own direction.** Into a tab holding one pane, right; into
  one already holding two or more, down — three narrow columns is worse than a
  stacked pane, and both the board and an agent read fine at reduced height.
  Only the target tab is counted, not the whole workspace. This governs the
  board's own pane and dispatched agent panes alike; force it with
  `[defaults] split_direction = "right" | "down"`. `[defaults] max_panes_per_tab`
  (default 3, the board excluded) sets how full a tab gets before an agent opens
  a tab of its own — a trade between an unreadable column and an agent in a tab
  you will not notice.
- **A dispatched agent gets a workspace of its own.** `herdr worktree create`
  cuts the checkout, opens it as a workspace, and groups it under the parent
  repo in the spaces sidebar — the same shape as any worktree you open by hand.
  That is where an agent belongs: not a sliver of the tab you are working in,
  and not a tab you will not notice. `g` on the board jumps straight to it.
  Concurrency is still counted against the *routed* workspace from
  `routing.toml`, which is unaffected by the agent having its own space.
- **One branch, one worktree, and worktrees are never auto-removed.** git allows
  a branch in only one worktree, so a retry cannot cut a second checkout of the
  same branch — the previous attempt's is still holding it. Dispatch reopens
  that workspace, which is also the behaviour you want: a retry continues the
  work rather than starting beside it. Clearing them out is a thing you ask for:
  `herdr-board gc`.
- **`agent start` races the shell it is given.** `tab create` returns as soon as
  the pane exists, but `agent start` needs the shell at its prompt owning the
  foreground; starting immediately gets `agent_pane_busy` and leaves an empty
  terminal with no agent. There is no readiness signal to wait on, so dispatch
  retries for a few seconds.
- **Dispatch is handed to a detached child.** Worktree, pane and agent startup
  take seconds, and doing them inline froze the picker while the operator
  watched a terminal boot. The picker spawns the work and closes; results come
  back through the database and surface as a board message.
- **`agent prompt` is sent without `--wait`.** Blocking would hold the picker
  open for the length of an agent turn; the daemon reconciles instead.
- **The board is a split, not a tab or an overlay.** A tab pins a global queue
  inside one workspace's layout; an overlay covers the work you are consulting
  the board about. `--placement split --direction right` is passed at open time
  because the manifest has no direction field. Splits are ordinary herdr panes
  once open, so the board has a pane id and can be closed by id — unlike the
  picker popup.
- **A recorded pane id is a hint, not proof.** Pane ids are reused, so the
  toggle confirms a pane is ours by the label herdr sets from the manifest title
  before closing it — otherwise a stale note eventually points at somebody
  else's pane. `plugin pane close` also only knows panes of a *currently
  registered* plugin, so a board left over from an earlier plugin id needs the
  ordinary `pane close`.
- **A key binding can only fire a `plugin_action`,** not `plugin pane open`
  directly, so the toggle is an action (`board.toggle`) that opens or
  closes the overlay itself.
- **Pane commands are resolved through `PATH`, not the plugin root.** Despite
  runtime commands running with the plugin directory as their working directory,
  a bare `target/release/herdr-board` fails to spawn with *"No viable candidates
  found in PATH"*. The manifest uses `./target/release/herdr-board`; the leading
  `./` is load-bearing.

The raw socket (`HERDR_SOCKET_PATH`) is never used — everything is expressible
through the CLI via `HERDR_BIN_PATH`, which is also what keeps this portable.

The `[[events]] on = "pane.exited"` hook is an optimization only: it shortens the
delay before a vanished pane is noticed. The poll loop remains authoritative and
is sufficient on its own.

## Decisions worth knowing

These were underspecified or contradictory across the two source specs, and were
resolved here rather than guessed at repeatedly.

- **A cancelled task returns to `ready`.** The derivation matrix did not cover
  "upstream `started`, no live attempt, no PR". Cancelling ends the attempt, not
  the issue — the work is still owed.
- **`d mark done` writes a local override.** State is derived, so without one the
  next poll would recompute `open` upstream and the row would come straight back:
  a key that undoes itself. The override survives re-derivation and is cleared by
  a retry. This also makes `d` honest on GitHub rows while GitHub stays read-only.
- **Open pull requests are rows, not just signals.** The impl spec treated a PR
  as something that flips an existing task to `review`. That leaves every PR
  nobody dispatched invisible — and an open PR is the definition of work waiting
  on you. Open PRs in configured repos now appear as their own `review` rows,
  ids using `!` (`gh:owner/repo!508`) so a PR and an issue of the same number
  stay distinct. A PR whose branch belongs to an attempt still attaches to that
  task instead, so dispatched work never appears twice. Turn it off with
  `[github] pull_requests = false`.
- **GitHub writeback is opt-in.** The board can leave the same trail on GitHub
  that Linear gets — a comment on dispatch and on outcome, and closing the issue
  on done — but it is **off by default**: pointing the board at a repo is not
  the same as asking it to write to your issues. `d mark done` stays honest
  without it, because the local override moves the row and survives
  re-derivation; it just does not close the issue upstream. `doctor` states
  which posture you are in. As originally specced, `d mark done` moved a
  GitHub row and the next poll recomputed `open` upstream and moved it straight
  back — a key that undoes itself. The board now leaves the same trail on GitHub
  that Linear gets: a comment on dispatch and on outcome, and **close on done**.
  In-flight state (`working`/`blocked`/`review`) still has nowhere to live
  upstream and does not need to: it is derived from the live attempt in
  `board.db`, and upstream only has to carry the terminal state. Set
  `[github] writeback = false` to keep it strictly read-only. The loop guard
  carries across — there are two writers on one issue now, the board and the
  agent in the pane.
- **Branch template follows the impl spec** (`board/{identifier_lower}` →
  `board/lin-145`), not the design fixtures. It is config either way.
- **Daemon liveness and source freshness are separate clocks.** During an outage
  the daemon keeps cycling on time while the sources go stale, so the header
  shows a live daemon and `last synced 4m`. Conflating them would report a dead
  daemon every time Linear hiccuped.
- **The board draws no vertical rules at all.** The design handoff flagged
  unbroken vertical rules as an open risk that the HTML prototype could not
  settle. The preferred resolution there was to draw none — herdr already owns
  pane chrome via `ui.pane_borders`, and drawing our own would double every
  divider. A test asserts no `│` is ever emitted, on every screen at every size,
  so the font question never arises.

## Development

```bash
cargo test                 # 272 tests (258 unit, 14 integration)
cargo clippy --all-targets -- -D warnings
cargo run -- demo --list   # every board state, no network or database
cargo run -- demo linear-down
```

The demo covers: populated, empty, source-down, syncd-dead and stale-binding,
including the `no route` row, the idle working row, both confirmations and the
outcome lines that follow them, and all four screens. `n` cycles scenarios, and
the mouse works exactly as it does on the real board — a demo that ignores
clicks cannot be used to review the mouse.

### Looking at it

Some of the design's rules can only be settled by looking: that it survives a
light terminal, that it survives a colourless one, and that the mouse really
does what the keyboard does. `tools/render-check/` drives the demo in a live
herdr pane, captures what it emits, and re-renders those captures under a dark,
a light and a monochrome palette — plus a parity script that performs each
action twice, once from the keyboard and once from a real mouse report, and
diffs the screens. See `tools/render-check/README.md`.

Worth knowing before you reach for a herdr theme: a pane app receives the
**host terminal's** ANSI palette. Switching `[theme] name` restyles herdr's own
chrome and leaves the board's sixteen colours untouched, so the palette to vary
is the terminal's.

Tests cover the state-derivation matrix, routing resolution, writeback
idempotency, reconciliation and orphan handling, dispatch provenance, schema
migration from an older database, the full key map, and cell-exact rendering at
80×24, 120×40, 160×38 and 60×20. `tests/sync_once.rs` drives whole
sync cycles against recorded Linear responses in `tests/fixtures/`, and
`tests/gc_worktree.rs` runs `gc` against a real repository — the checkout goes,
the branch survives, a checkout with uncommitted work is refused, and both
removal paths are exercised by faking what herdr reports.

### Tasks that vanish upstream

An incremental poll cannot see a deletion: a deleted issue is simply never
returned again, which is indistinguishable from one that has not changed. So
every two minutes Linear is polled *without* the watermark, and anything of ours
missing from that complete response is reaped. GitHub is always polled in full,
so it reaps every cycle — but only when every configured repo answered, since a
failed poll would otherwise look like an emptied repo.

What reaping does depends on whether anyone ever worked on the task:

- **Never dispatched** — the row was noise, an issue created and deleted again,
  and it is forgotten outright, with any queued writebacks.
- **It has attempts** — the row stays, marked `gone` upstream, and derives to
  `done`. The attempts stay on it: which agent ran, on what branch, in what
  worktree, and how it ended.

Keeping the row is not only record-keeping. The attempt row is the only thing
that knows where a checkout came from, so deleting it stranded the worktree —
`gc` would report the directory as untracked and refuse to touch it, and it
leaked until removed by hand. A `gone` row keeps the checkout attributable, and
because `gone` is terminal the checkout ages out through `gc` like any other.

A `gone` row cannot be dispatched — there is no issue behind it to work on, move
or comment on — and `dispatch` says so rather than offering a route. Queued
writebacks against it are dropped, since a comment aimed at a deleted issue
would fail and back off forever; already-delivered rows stay, so their
idempotency keys still hold if the issue ever comes back. If it does come back,
the next poll upserts it and the row returns to life with its history intact.

A task with a live attempt is never reaped at all: an agent is working on it,
and the row vanishing from under a running pane is worse than a stale row.

### Schema changes

`board.db` is migrated in place on open: missing columns are added, so upgrading
the binary does not strand an existing database. This matters more than it
sounds — a missing column makes `load_tasks` fail, which takes the board pane
down with it.

### Clearing out old worktrees

Nothing removes a worktree on its own, and that is deliberate — see "one branch,
one worktree" above. Checkouts therefore accumulate one per attempt and never
shrink, so there is a sweep you run when you want the space back:

```bash
herdr-board gc --dry-run           # what would go, and why the rest stays
herdr-board gc                     # remove them
herdr-board gc --older-than 2w     # 14d by default; also 36h, 2w
```

A checkout goes only when it is **terminal and aged** — both, because either one
alone deletes work someone is coming back for:

- **Terminal** is a property of the task, not the attempt. Only `done` counts. A
  `cancelled` attempt puts its row back in `ready` and `failed` is the state you
  retry from, and a retry reuses the checkout that already holds the branch — so
  those stay however old they get. `--dry-run` prints the reason against each.
- **Aged** is the last attempt in that checkout having ended longer ago than
  `--older-than`. `--older-than` always needs a unit: a bare `14` means seconds
  to `[sync] interval`, and reading it that way here would collect everything.

**The branch is never touched.** Removing the checkout is what frees the branch
for a fresh worktree; deleting it would throw the work away. A checkout with
uncommitted changes is refused and reported rather than forced, and gc exits
non-zero when that happens.

**gc asks herdr where the checkouts are.** Dispatch hands the checkout to
`herdr worktree create`, so herdr is what knows where it went — under its
`[worktrees] directory`, which is the operator's to configure, and grouped by
repo. There is no directory of ours left to walk, so gc runs
`herdr worktree list` against each repository the routes name. That also covers
the `$STATE/wt/` an older version used, so a board running across the change
collects from both.

Which leaves two ways a checkout comes back, and gc does both:

- **herdr still holds it open as a workspace** — `herdr worktree remove` runs
  `git worktree remove` *and* closes the workspace. git alone would leave herdr
  showing a space with no directory behind it.
- **Nothing holds it** — the common case, because a workspace whose last pane
  closes is dropped: cancelling an attempt takes the workspace with it and
  leaves the checkout still holding its branch. There is no workspace left to
  name, so git removes it. A checkout deleted by hand leaves git's
  administrative entry behind, still holding the branch against a retry, and
  `git worktree prune` clears that.

A checkout on a branch the board cuts — `branch_template`'s literal head,
`board/` by default — that no attempt claims is **listed, never removed**:
nothing on the board can say whether it is still wanted, so you decide. The
branch filter is what keeps that list short. A repo the board works in is one
you work in too, and herdr lists every checkout of it; without the filter your
own worktrees, and the ones other agents cut under `.claude/worktrees/`, bury
the one line that matters. Reaping used to be how strays appeared and no longer
is — a reaped task keeps its attempts, so its checkout stays claimed and
collectable.

### Workspace concurrency is not on the board

Nothing in the list says how full a workspace is. A `ws:offhand` with room and
one at three of three read identically, and the design nominates the `WORKING`
section header as the place to fix that. It is deliberately not built, and this
is the record of why — not a note to build it later.

The premise is narrower than it sounds. The picker states capacity on **every**
dispatch, not only on a refusal: `ws:offhand  2 of 3 working`, dim, on the row
above the footer, before `enter` is pressed. At cap the same line goes loud and
`enter` leaves the footer entirely. So the number is one keypress from the row
you are asking about, and it is never absent at the moment it decides anything.

Three things real use has to settle before a header can be written:

- **Which workspace.** Routes each name their own, so `WORKING` routinely holds
  rows from several at once and the header is one line. `ws:offhand 1/3
  ws:fintech 3/3` at 80 columns is a legend, not a status. The alternative —
  following the selected row's workspace — is a header that changes as you
  arrow down the list, which is worse than one that says nothing.
- **Whether it carries information.** Below cap it is a fact already on screen:
  every working row prints its own `ws:` at 60 columns and up, so the count is
  there to be counted. It says something new only at cap, which is precisely
  where the picker already says it.
- **Whether the cap is the interesting number.** What would actually sting is
  opening a dispatch *expecting* room and being told no. Designing against that
  surprise and hanging a permanent counter on the board are different features,
  and at most one of them is warranted.

Building it later is cheap, which is the other half of why deferring costs
nothing here. The live per-workspace count is already in the view model —
`TaskView::workspace` is on every row — and the cap is not: `App` holds no
`RoutingConfig`, so `max_concurrent_per_workspace` has to be threaded in beside
the views. That is the whole cost.

**The trigger to revisit:** the first dispatch opened expecting room and
refused. Until that has happened, a header is a guess at a problem, and a
section header is one line the board only gets to spend once.

### Not in v0

Multi-operator anything, Linear webhooks and agent sessions (polling is enough
for one person), and auto-dispatch rules.
