//! Cell-exact rendering of the four board screens.
//!
//! The character grid is the spec. Column positions, glyphs, truncation points
//! and copy are reproduced exactly; colors resolve through [`super::theme`].
//!
//! **This module draws no `│`.** herdr owns pane borders and dividers via
//! `ui.pane_borders` / `ui.pane_gaps`; drawing our own would give the operator
//! doubled rules. Only horizontal `─` separators appear here.

use super::state::*;
use super::theme;
use crate::model::{BoardState, UpstreamState};
use crate::sync::SourceHealth;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

// The character grid (design spec: "Spacing is a character grid").
pub const COL_GUTTER: u16 = 1;
pub const COL_ID: u16 = 3;
pub const ID_WIDTH: u16 = 8;
pub const COL_TITLE: u16 = 13;
/// Below this width, drop all metadata rather than wrap.
pub const NARROW_LIMIT: u16 = 60;
/// At or above this width an agent-dispatched row shows runtime *and*
/// provenance; below it, provenance takes the runtime column.
pub const WIDE_LIMIT: u16 = 100;
/// The title never comes within this many cells of the metadata block.
pub const TITLE_METADATA_GAP: u16 = 2;

// ---- primitives --------------------------------------------------------

/// Write clipped to `area`. Returns the width actually consumed.
pub fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) -> u16 {
    if y >= area.height {
        return 0;
    }
    let max = area.width.saturating_sub(x);
    if max == 0 {
        return 0;
    }
    let s = truncate(text, max as usize);
    buf.set_string(area.x + x, area.y + y, &s, style);
    s.chars().count() as u16
}

/// Right-align `text` so it ends at column `right` (inclusive).
pub fn put_right(buf: &mut Buffer, area: Rect, right: u16, y: u16, text: &str, style: Style) -> u16 {
    let w = text.chars().count() as u16;
    let x = (right + 1).saturating_sub(w);
    put(buf, area, x, y, text, style);
    x
}

/// A horizontal rule from `x1` to `x2` inclusive.
pub fn rule(buf: &mut Buffer, area: Rect, x1: u16, x2: u16, y: u16, style: Style) {
    if x2 < x1 {
        return;
    }
    let s: String = std::iter::repeat_n(theme::RULE, (x2 - x1 + 1) as usize).collect();
    put(buf, area, x1, y, &s, style);
}

/// Truncate to `max` display cells, marking the cut with `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push(theme::ELLIPSIS);
    out
}

/// Pad or truncate to an exact column width.
fn fixed(s: &str, width: usize) -> String {
    let t = truncate(s, width);
    let n = t.chars().count();
    format!("{t}{}", " ".repeat(width.saturating_sub(n)))
}

/// Reverse a whole row as one solid bar.
///
/// Per-cell reversal against each cell's own foreground produces a mottled bar
/// (white behind the title, grey behind the metadata). So: strip `DIM` and reset
/// every foreground first — **except the gutter cell**, which keeps its state
/// color and reads as a colored cursor block.
pub fn reverse_row(buf: &mut Buffer, area: Rect, y: u16, gutter_x: u16) {
    if y >= area.height {
        return;
    }
    for x in 0..area.width {
        let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) else {
            continue;
        };
        if x != gutter_x {
            cell.fg = Color::Reset;
        }
        cell.modifier.remove(Modifier::DIM);
        cell.modifier.insert(Modifier::REVERSED);
    }
}

// ---- header ------------------------------------------------------------

/// Sync status as **text**, never a spinner.
///
/// Three distinct states: healthy, a down source, and a dead daemon. The third
/// is not the second — the sources may be fine and nobody is asking them.
pub fn header_status(sync: &SyncStatus) -> String {
    if sync.syncd_dead() {
        let age = sync
            .last_cycle_secs
            .map(format_age)
            .unwrap_or_else(|| "never".into());
        return format!("syncd not running   sync stale {age}");
    }

    let mut parts: Vec<String> = Vec::new();
    match &sync.linear {
        SourceHealth::Absent => {}
        SourceHealth::Ok => parts.push("linear ✓".into()),
        SourceHealth::Down { retry_in, .. } => {
            parts.push(format!("linear ✗ retrying {retry_in}s"))
        }
    }
    match &sync.github {
        SourceHealth::Absent => {}
        SourceHealth::Ok => parts.push("gh ✓".into()),
        SourceHealth::Down { retry_in, .. } => parts.push(format!("gh ✗ retrying {retry_in}s")),
    }

    let degraded = matches!(sync.linear, SourceHealth::Down { .. })
        || matches!(sync.github, SourceHealth::Down { .. });
    // With no source configured none has ever "answered", so fall back to the
    // cycle age — otherwise the header reads a bare `synced` with no number.
    let age = sync
        .last_source_ok_secs
        .or(sync.last_cycle_secs)
        .map(format_age)
        .unwrap_or_else(|| "never".into());
    parts.push(if degraded {
        format!("last synced {age}")
    } else {
        format!("synced {age}")
    });
    parts.join("   ")
}

/// How loudly to say it.
///
/// Healthy is dim, like every other piece of chrome. A down source or a dead
/// daemon is not chrome — it is the one thing on the board that says the board
/// is lying to you, and dim is the quietest style there is. Seen on a light
/// terminal, where dim collapses towards the background, `linear ✗ retrying 30s`
/// read exactly as quietly as `gh ✓`.
///
/// Red carries it on a colour terminal and bold carries it without one, so the
/// distinction survives being stripped either way.
pub fn header_style(sync: &SyncStatus) -> Style {
    let degraded = sync.syncd_dead()
        || matches!(sync.linear, SourceHealth::Down { .. })
        || matches!(sync.github, SourceHealth::Down { .. });
    if degraded {
        theme::state_style(BoardState::Blocked).add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    }
}

/// Below this much room, the sync status is dropped rather than shortened into
/// something unreadable.
const MIN_STATUS: u16 = 10;

/// The header, with the sync status and the active filter sharing the corner.
///
/// The filter takes the corner outright and the status gives way to it as the
/// pane narrows. That order is deliberate: the status is a fact about the world
/// that will still be true in a minute, and the filter is a thing the operator
/// just did to the board in front of them. A board hiding most of its rows and
/// not saying why looks broken.
fn render_header(
    buf: &mut Buffer,
    area: Rect,
    left: &str,
    sync: Option<&SyncStatus>,
    filter: Option<&str>,
) {
    let used = put(buf, area, 1, 0, left, theme::fg_default());
    // A split can make the board much narrower than 80 cells. Both of these are
    // right-aligned, so without a clamp they write straight over the title —
    // and a header that reads `herdr-bgh ✗ retrying 20s` is worse than one with
    // no status at all.
    let mut right = area.width.saturating_sub(2);
    let mut room = right.saturating_sub(1 + used).saturating_sub(2);

    if let Some(f) = filter {
        let f = truncate(f, room as usize);
        let w = f.chars().count() as u16;
        if w > 0 {
            // Default fg, not dim: every other thing up here is chrome, and
            // this one is state the operator can change with a keypress.
            put_right(buf, area, right, 0, &f, theme::fg_default());
            // Three cells, the gap the status already uses between its parts.
            right = right.saturating_sub(w + 3);
            room = room.saturating_sub(w + 3);
        }
    }

    if let Some(s) = sync
        && room >= MIN_STATUS
    {
        let status = truncate(&header_status(s), room as usize);
        put_right(buf, area, right, 0, &status, header_style(s));
    }
    rule(
        buf,
        area,
        1,
        area.width.saturating_sub(2),
        1,
        theme::dim(),
    );
}

// ---- metadata ----------------------------------------------------------

/// The right-hand metadata block for a row, already collapsed for `width`.
///
/// Narrowing collapses right-to-left: elapsed survives longest, runtime drops
/// first. Below 60 columns everything goes rather than wrapping.
pub fn metadata(v: &TaskView, selected: bool, width: u16) -> String {
    if width < NARROW_LIMIT {
        return String::new();
    }
    match v.state() {
        BoardState::Working | BoardState::Blocked => {
            let elapsed = if v.idle {
                // The task is still `working`; a bare ticking counter would
                // imply motion that is not happening. No new state, no new
                // glyph — just this marker.
                format!("{:>12}", format!("idle {}", format_elapsed(v.elapsed_secs.unwrap_or(0))))
            } else {
                format!("{:>6}", format_elapsed(v.elapsed_secs.unwrap_or(0)))
            };
            let runtime = fixed(v.runtime.as_deref().unwrap_or(""), 12);
            let workspace = fixed(&ws(v), 11);
            let via = v.dispatched_by.as_deref().map(|p| format!("via {p}"));

            // When an agent chose the runtime, which one it picked is the least
            // interesting fact on the row and the parent is the most — so at
            // ordinary widths provenance takes the runtime column outright,
            // costing zero title width. Only from 100 columns is there room for
            // both, and the runtime is always in detail regardless.
            let full = match &via {
                Some(via) if width >= WIDE_LIMIT => {
                    format!("{runtime}{workspace}{}{elapsed}", fixed(via, 13))
                }
                Some(via) => format!("{}{workspace}{elapsed}", fixed(via, 12)),
                None => format!("{runtime}{workspace}{elapsed}"),
            };

            let avail = available_metadata_width(width);
            if full.chars().count() as u16 <= avail {
                return full;
            }
            // Narrowing collapses right-to-left. Provenance outranks the
            // runtime it displaced, so it is what survives into the leading
            // column.
            let lead = match &via {
                Some(via) => fixed(via, 12),
                None => runtime,
            };
            let without_lead = format!("{workspace}{elapsed}");
            if without_lead.chars().count() as u16 <= avail {
                without_lead
            } else {
                let _ = lead;
                elapsed.trim_start().to_string()
            }
        }
        BoardState::Failed => "pane exited without completing".into(),
        BoardState::Review => match (v.task.pr_number, v.branch.as_deref()) {
            // A merged PR means the work landed and only the ticket is left —
            // otherwise the row sits in `review` saying nothing about why.
            (Some(n), _) if v.task.pr_merged => format!("PR #{n} merged · close the issue"),
            // With several branches in flight, whether it can still merge is
            // the most useful thing to say about a PR waiting on you.
            (Some(n), _) => {
                let repo = match v.repo.as_deref() {
                    Some(r) => format!("{r} "),
                    None => String::new(),
                };
                match v.task.pr_mergeable.as_deref() {
                    Some("behind") => format!("{repo}PR #{n} · behind master"),
                    Some("dirty") => format!("{repo}PR #{n} · conflicts"),
                    Some("blocked") => format!("{repo}PR #{n} · blocked"),
                    Some("unstable") => format!("{repo}PR #{n} · check failing"),
                    _ => format!("{repo}PR #{n} · waiting on you"),
                }
            }
            // Finished on commits with no PR raised: say which branch, or the
            // row reads as "waiting on you" with nowhere to look.
            (None, Some(b)) => format!("{b} · no PR"),
            (None, None) => "waiting on you".into(),
        },
        BoardState::Ready => {
            // Where it would go, not where it came from.
            //
            // The repo answers this for a GitHub row, but a Linear row has no
            // repo and was left running to the edge while its neighbours
            // stopped short. The routed workspace is the same fact for both —
            // and it is the routing outcome, which is otherwise invisible until
            // the picker opens.
            let repo = v
                .workspace
                .as_deref()
                .or(v.repo.as_deref())
                .unwrap_or_default();
            if !v.has_route {
                // A property of the issue, not an affordance for the cursor —
                // so it shows on every such row, selected or not.
                if repo.is_empty() {
                    "no route".into()
                } else {
                    format!("{repo} · no route")
                }
            } else if selected {
                // Repeating this on every ready row is four copies of one
                // sentence.
                if repo.is_empty() {
                    "[enter to dispatch]".into()
                } else {
                    format!("{repo} · [enter to dispatch]")
                }
            } else {
                repo.to_string()
            }
        }
        BoardState::Done => {
            // A row whose issue was deleted sits in `done` next to rows that
            // were properly closed, and the two are worth telling apart: this
            // one is here because the issue went away, not because the work
            // landed. The attempts are still on it, which is the point.
            if v.task.upstream == UpstreamState::Gone {
                return format!("{}{}", fixed(v.runtime.as_deref().unwrap_or(""), 12), "gone upstream");
            }
            format!("{}{}", fixed(v.runtime.as_deref().unwrap_or(""), 12), fixed(&ws(v), 11))
        }
    }
}

fn ws(v: &TaskView) -> String {
    v.workspace
        .as_deref()
        .map(|w| format!("ws:{w}"))
        .unwrap_or_default()
}

/// How much room the metadata block may take before it would crowd the title.
fn available_metadata_width(width: u16) -> u16 {
    width
        .saturating_sub(COL_TITLE)
        .saturating_sub(TITLE_METADATA_GAP)
        // Leave the title at least a readable stub.
        .saturating_sub(12)
}

// ---- list --------------------------------------------------------------

pub fn render_list(buf: &mut Buffer, area: Rect, app: &mut App) {
    app.rows.clear();
    app.footer_hits.clear();
    let filter = app.header_filter();
    render_header(buf, area, "herdr-board", Some(&app.sync), filter.as_deref());

    let last = area.height.saturating_sub(1);
    let body_bottom = last.saturating_sub(2); // rule sits at last-1
    let mut more_marker: Option<String> = None;

    // An empty queue does not mean an empty body: a board with no tasks and a
    // repo nobody has adopted is exactly the case this exists for, and the
    // section has to draw underneath the empty copy rather than instead of it.
    let body_top = if app.is_empty() {
        render_empty(buf, area, 2, app)
    } else {
        2u16
    };

    // Flatten to the full list of lines the body would draw, then take a window
    // of it. Without this the body is simply clipped: a selection below the fold
    // is invisible and unreachable, which on a short pane is most of the board.
    let lines: Vec<Row> = app.rows();

    // A board with rows on it, showing none of them, has to say which of the
    // operator's own keypresses did that — the header names the filter, and
    // this names the way out. A board with no rows at all is a different
    // sentence, already written above.
    if lines.is_empty()
        && !app.is_empty()
        && let Some(note) = app.filter.empty_note()
        && body_top <= body_bottom
    {
        put(buf, area, 1, body_top, &note, theme::fg_default());
    }

    if body_top <= body_bottom {
        let height = (body_bottom + 1).saturating_sub(body_top) as usize;
        app.scroll = clamp_scroll(&lines, app.selected_id.as_deref(), app.scroll, height);

        let mut y = body_top;
        for row in lines.iter().skip(app.scroll).take(height) {
            let selected = app.selected_id.as_deref() == Some(row.id().as_str());
            match row {
                Row::Section(state) => {
                    render_section_header(
                        buf,
                        area,
                        y,
                        *state,
                        app.is_collapsed(*state),
                        app.section_len(*state),
                    );
                    if selected {
                        reverse_row(buf, area, y, COL_GUTTER);
                    }
                }
                Row::Task(id) => {
                    let Some(v) = app.views.iter().find(|v| v.id() == id) else {
                        continue;
                    };
                    render_task_row(buf, area, y, v, selected);
                }
                Row::UnadoptedSection => {
                    // What is shown, not what exists: the count on a folded
                    // header has to agree with the rows unfolding it produces.
                    let shown = app.unadopted_shown();
                    let missing: Vec<crate::adopt::Missing> =
                        shown.iter().map(|u| u.missing).collect();
                    render_unadopted_header(
                        buf,
                        area,
                        y,
                        app.collapsed_unadopted,
                        shown.len(),
                        &missing,
                    );
                    if selected {
                        reverse_row(buf, area, y, COL_GUTTER);
                    }
                }
                Row::Unadopted(slug) => {
                    let Some(u) = app.unadopted.iter().find(|u| &u.slug == slug) else {
                        continue;
                    };
                    render_unadopted_row(buf, area, y, u, selected);
                }
            }
            app.rows.push((y, row.clone()));
            y += 1;
        }

        // A scrollable list has to say so, or a short pane looks like the whole
        // board. Drawn after the rule below, which would otherwise paint over it.
        more_marker = scroll_marker(lines.len(), app.scroll, height);
    }

    rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
    if let Some(marker) = &more_marker {
        put_right(buf, area, area.width.saturating_sub(2), last - 1, marker, theme::dim());
    }
    render_footer(buf, area, last, app);
}

/// Draw the empty state and return the first row still free beneath it.
fn render_empty(buf: &mut Buffer, area: Rect, y: u16, app: &App) -> u16 {
    // Two different empty states. A configured board with nothing on it says so
    // and stops; a board that was never set up says what is missing, because
    // naming a config file the operator has not created is not an instruction.
    if app.setup_hints.is_empty() {
        put(
            buf,
            area,
            1,
            y,
            "Nothing queued. Issues assigned to you in Linear appear here.",
            theme::fg_default(),
        );
        put(buf, area, 1, y + 2, &app.config_path, theme::dim());
        put(
            buf,
            area,
            1,
            y + 3,
            "maps issues to a workspace and a runtime.",
            theme::dim(),
        );
        return y + 5;
    }

    put(
        buf,
        area,
        1,
        y,
        "Nothing queued, because the board is not set up yet.",
        theme::fg_default(),
    );
    let mut row = y + 2;
    for hint in &app.setup_hints {
        if row >= area.height.saturating_sub(3) {
            break;
        }
        put(
            buf,
            area,
            1,
            row,
            &truncate(hint, area.width.saturating_sub(2) as usize),
            theme::dim(),
        );
        row += 1;
    }
    row + 1
}

/// The glyph for a repo the board is not watching.
///
/// A seventh shape, distinct from all six state glyphs: an unadopted repo is
/// not a state a task can be in, and reading it as one would be worse than not
/// showing it. Like every other gutter it has to survive color being stripped,
/// so the shape carries it.
pub const UNADOPTED_GLYPH: &str = "⌾";

fn render_unadopted_header(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    collapsed: bool,
    len: usize,
    missing: &[crate::adopt::Missing],
) {
    put(buf, area, COL_GUTTER, y, UNADOPTED_GLYPH, theme::fg_default());
    let used = put(buf, area, COL_ID, y, "UNADOPTED  ", theme::bold());
    let hint = if collapsed {
        format!("{len} hidden · enter to expand  ")
    } else {
        // Says what the section *is* rather than counting rows you can see —
        // the same rule the state headers follow. Only what every row shares:
        // a fixed "not polled, not dispatchable" contradicted any row that was
        // missing just one of the two.
        crate::adopt::Missing::header_note(missing).to_string()
    };
    let used2 = put(buf, area, COL_ID + used, y, &hint, theme::dim());
    rule(
        buf,
        area,
        COL_ID + used + used2,
        area.width.saturating_sub(2),
        y,
        theme::dim(),
    );
}

/// `⌾ tripletex-mcp                        Florin-AS/tripletex-mcp`
///
/// The workspace label reads as the title, because it is the name you recognise
/// from the spaces sidebar; the slug is metadata, because it is what gets
/// written to config. Same hierarchy as a task row.
fn render_unadopted_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    u: &crate::adopt::Unadopted,
    selected: bool,
) {
    put(buf, area, COL_GUTTER, y, UNADOPTED_GLYPH, theme::fg_default());

    let meta = format!("{}{}", u.slug, u.missing.note());
    let right = area.width.saturating_sub(2);
    let meta_w = meta.chars().count() as u16;
    // Below the narrow limit the slug goes rather than the name: the name is
    // what identifies the row, and a truncated slug identifies nothing.
    let show_meta = area.width >= NARROW_LIMIT && meta_w + COL_ID + 12 < right;
    let name_max = if show_meta {
        right
            .saturating_sub(meta_w)
            .saturating_sub(TITLE_METADATA_GAP)
            .saturating_sub(COL_ID)
            + 1
    } else {
        right.saturating_sub(COL_ID) + 1
    };
    put(
        buf,
        area,
        COL_ID,
        y,
        &truncate(&u.label, name_max as usize),
        theme::fg_default(),
    );
    if show_meta {
        put_right(buf, area, right, y, &meta, theme::dim());
    }
    if selected {
        reverse_row(buf, area, y, COL_GUTTER);
    }
}

/// A section header must read as a different *shape* from a row, not just a
/// different color: glyph, bold uppercase label, then a dim rule to the margin.
///
/// No count when it is open — it competed with the elapsed column and told the
/// operator nothing they could not see. Folded is the opposite case: the rows
/// are gone, so the number is the only thing left that says what is in there.
fn render_section_header(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    state: BoardState,
    collapsed: bool,
    len: usize,
) {
    put(buf, area, COL_GUTTER, y, state.glyph(), theme::state_style(state));
    let label = if state == BoardState::Done {
        "DONE today  ".to_string()
    } else {
        format!("{}  ", state.label())
    };
    let used = put(buf, area, COL_ID, y, &label, theme::bold());
    let hint = if collapsed {
        format!("{len} hidden · enter to expand  ")
    } else {
        String::new()
    };
    let used2 = put(buf, area, COL_ID + used, y, &hint, theme::dim());
    rule(
        buf,
        area,
        COL_ID + used + used2,
        area.width.saturating_sub(2),
        y,
        theme::dim(),
    );
}

/// Row hierarchy, in order of loudness: gutter (color) → title (default fg) →
/// id and metadata (dim). The gutter is the only colored cell in the row.
fn render_task_row(buf: &mut Buffer, area: Rect, y: u16, v: &TaskView, selected: bool) {
    let state = v.state();
    put(buf, area, COL_GUTTER, y, state.glyph(), theme::state_style(state));
    put(
        buf,
        area,
        COL_ID,
        y,
        &truncate(&v.task.identifier, ID_WIDTH as usize),
        theme::dim(),
    );

    let meta = metadata(v, selected, area.width);
    let meta_w = meta.chars().count() as u16;
    let right = area.width.saturating_sub(2);
    let title_max = if meta_w == 0 {
        right.saturating_sub(COL_TITLE) + 1
    } else {
        right
            .saturating_sub(meta_w)
            .saturating_sub(TITLE_METADATA_GAP)
            .saturating_sub(COL_TITLE)
            + 1
    };

    // Closed issues collapse to a dim line, title included.
    let title_style = if state == BoardState::Done {
        theme::dim()
    } else {
        theme::fg_default()
    };
    put(
        buf,
        area,
        COL_TITLE,
        y,
        &truncate(&v.task.title, title_max as usize),
        title_style,
    );
    if meta_w > 0 {
        put_right(buf, area, right, y, &meta, theme::dim());
    }

    if selected {
        reverse_row(buf, area, y, COL_GUTTER);
    }
}

// ---- footer ------------------------------------------------------------

/// Keys relevant to *the current selection*, not the full map.
///
/// Plus the way out of a filter whenever one is on, which is not about the
/// selection at all: `f` and `/` are learned from the help screen, but a board
/// showing a fraction of its rows has to offer the key that undoes that without
/// making you go and look it up.
pub fn footer_hints(app: &App) -> Vec<(&'static str, &'static str, char)> {
    let mut hints = selection_hints(app);
    if app.screen == Screen::List && app.filter.active() && !app.typing {
        hints.push(("F", "clear the filter", 'F'));
    }
    hints
}

fn selection_hints(app: &App) -> Vec<(&'static str, &'static str, char)> {
    match app.screen {
        // The reference is longer than a 24-row pane, so say how to see the
        // rest of it rather than letting the last groups look absent.
        Screen::Help if help_lines().len() > help_body_height(app.last_height) => vec![
            ("j k", "more keys", 'j'),
            ("esc", "back to the board", '\u{1b}'),
        ],
        Screen::Help | Screen::Stats => vec![("esc", "back to the board", '\u{1b}')],
        // How many rows `enter` would produce is on the body's right edge,
        // where it moves as you pick; the footer only has to offer the keys.
        Screen::Adopt => {
            let mut v = Vec::new();
            if !app.adopt.as_ref().is_none_or(|a| a.label_rows().is_empty()) {
                v.push(("space", "pick a label", ' '));
            }
            v.push(("enter", "adopt", '\r'));
            v.push(("esc", "cancel — nothing is written", '\u{1b}'));
            v.push(("?", "help", '?'));
            v
        }
        Screen::Detail => {
            let mut v = vec![
                ("h", "back", 'h'),
                ("g", "go to pane", 'g'),
                ("o", "open in browser", 'o'),
            ];
            match app.selected().map(|s| s.state()) {
                Some(BoardState::Working) | Some(BoardState::Blocked) => {
                    v.push(("x", "cancel", 'x'))
                }
                Some(BoardState::Failed) => {
                    v.push(("r", "retry", 'r'));
                    v.push(("d", "mark done", 'd'));
                }
                _ => {}
            }
            // Detail is where you go to decide; merging is the decision.
            if app.selected().is_some_and(|v| v.task.pr_number.is_some()) {
                v.push(("m", "merge", 'm'));
            }
            v.push(("?", "help", '?'));
            v
        }
        Screen::Prompt => {
            let mut v = Vec::new();
            match app.selected().map(|s| s.state()) {
                // On a working or blocked task there is no `enter` hint at all;
                // pressing it states the fact instead of doing nothing.
                Some(BoardState::Working) | Some(BoardState::Blocked) => {}
                Some(BoardState::Failed) => v.push(("enter", "retry", '\r')),
                _ => v.push(("enter", "dispatch", '\r')),
            }
            v.push(("p", "summary", 'p'));
            v.push(("h", "back", 'h'));
            v.push(("o", "open", 'o'));
            v.push(("?", "help", '?'));
            v
        }
        // The two keys the design sketches beneath the row itself. They live in
        // the footer instead, where every other affordance on this board lives —
        // a second line per row would be the only variable-height row on screen.
        Screen::List if app.unadopted_selected().is_some() => vec![
            ("a", "adopt — write the route and start polling", 'a'),
            ("X", "ignore", 'X'),
            ("?", "help", '?'),
        ],
        Screen::List if app.on_unadopted_header() => vec![
            (
                "enter",
                if app.collapsed_unadopted {
                    "expand"
                } else {
                    "collapse"
                },
                '\r',
            ),
            ("s", "sync", 's'),
            ("?", "help", '?'),
        ],
        Screen::List if app.on_section_header().is_some() => {
            let folded = app
                .on_section_header()
                .is_some_and(|s| app.is_collapsed(s));
            vec![
                ("enter", if folded { "expand" } else { "collapse" }, '\r'),
                ("s", "sync", 's'),
                ("?", "help", '?'),
            ]
        }
        Screen::List => match app.selected().map(|s| s.state()) {
            // `l detail` is a deliberate addition to the design's footer table.
            // On a ready row `enter` dispatches, so without this there is no
            // on-screen route to detail at all — and a board whose rows are all
            // ready (the normal state of a fresh board) offers no way to look at
            // anything before committing to it.
            Some(BoardState::Ready) => vec![
                ("enter", "dispatch", '\r'),
                ("l", "detail", 'l'),
                ("o", "open in browser", 'o'),
                ("s", "sync", 's'),
                ("?", "help", '?'),
            ],
            Some(BoardState::Working) => vec![
                ("enter", "detail", '\r'),
                ("g", "go to pane", 'g'),
                ("x", "cancel", 'x'),
                ("o", "open", 'o'),
                ("?", "help", '?'),
            ],
            Some(BoardState::Blocked) => vec![
                ("g", "go to pane", 'g'),
                ("enter", "detail", '\r'),
                ("o", "open", 'o'),
                ("?", "help", '?'),
            ],
            // Only promise a PR when there is one: work can reach `review` on
            // commits alone, and `o` then opens the issue.
            Some(BoardState::Review) => {
                let has_pr = app.selected().is_some_and(|v| v.task.pr_number.is_some());
                if has_pr {
                    vec![
                        ("m", "merge", 'm'),
                        ("o", "open PR", 'o'),
                        ("enter", "detail", '\r'),
                        ("?", "help", '?'),
                    ]
                } else {
                    vec![
                        ("o", "open issue", 'o'),
                        ("enter", "detail", '\r'),
                        ("?", "help", '?'),
                    ]
                }
            }
            Some(BoardState::Failed) => vec![
                ("r", "retry", 'r'),
                ("d", "mark done", 'd'),
                ("enter", "detail", '\r'),
                ("?", "help", '?'),
            ],
            Some(BoardState::Done) | None => vec![
                ("enter", "detail", '\r'),
                ("o", "open", 'o'),
                ("s", "sync", 's'),
                ("?", "help", '?'),
            ],
        },
    }
}

/// The `/` field: **a line, not a box**.
///
/// It replaces the footer the way the cancel confirmation does — the board has
/// one place where it talks to you, and a floating input would be the first
/// bordered thing on screen.
fn render_find_field(buf: &mut Buffer, area: Rect, y: u16, app: &App) {
    let q = match &app.filter {
        Filter::Text(q) => q.as_str(),
        _ => "",
    };
    let mut x = 1;
    x += put(buf, area, x, y, "/", theme::dim());
    x += put(buf, area, x, y, q, theme::fg_default());
    // The caret is one reversed cell — the same emphasis the selected row uses,
    // so it survives colour being stripped.
    put(
        buf,
        area,
        x,
        y,
        " ",
        Style::default().add_modifier(Modifier::REVERSED),
    );

    // What the query has found so far, so you can stop typing when it is down
    // to one row rather than typing the whole identifier every time.
    let n = app.shown_tasks();
    let count = format!("{n} row{} · esc clears", if n == 1 { "" } else { "s" });
    if area.width.saturating_sub(2) > x + count.chars().count() as u16 + 2 {
        put_right(buf, area, area.width.saturating_sub(2), y, &count, theme::dim());
    }
}

fn render_footer(buf: &mut Buffer, area: Rect, y: u16, app: &mut App) {
    // Typing outranks both: the field is where the keys are going.
    if app.typing {
        render_find_field(buf, area, y, app);
        return;
    }
    // A transient message and the cancel confirmation both replace the footer
    // inline rather than opening a second modal.
    if let Some(id) = &app.confirm {
        let merging = id.starts_with(super::app::MERGE_PREFIX);
        let bare = id.trim_start_matches(super::app::MERGE_PREFIX);
        let v = app.views.iter().find(|v| v.id() == bare);
        let ident = v.map(|v| v.task.identifier.clone()).unwrap_or_default();
        let question = if merging {
            match v.and_then(|v| v.task.pr_number) {
                Some(n) => format!("merge PR #{n} for {ident}?  "),
                None => format!("merge {ident}?  "),
            }
        } else {
            format!("cancel {ident} and kill its pane?  ")
        };
        let mut x = 1;
        x += put(buf, area, x, y, &question, theme::fg_default());
        x += put(buf, area, x, y, "y", theme::fg_default());
        x += put(buf, area, x, y, " yes  ", theme::dim());
        x += put(buf, area, x, y, "n", theme::fg_default());
        put(buf, area, x, y, " no", theme::dim());
        return;
    }
    if let Some((msg, _)) = &app.message {
        put(buf, area, 1, y, msg, theme::fg_default());
        return;
    }

    let mut x = 1;
    for (i, (key, label, ch)) in footer_hints(app).into_iter().enumerate() {
        if i > 0 {
            x += put(buf, area, x, y, " · ", theme::dim());
        }
        let start = x;
        // Each hint renders as `key` in default fg + ` label` in dim.
        x += put(buf, area, x, y, key, theme::fg_default());
        x += put(buf, area, x, y, &format!(" {label}"), theme::dim());
        // Every hint is a click target that fires its own action.
        app.footer_hits.push((start, x, ch));
    }
}

// ---- detail ------------------------------------------------------------

pub fn render_detail(buf: &mut Buffer, area: Rect, app: &mut App) {
    app.footer_hits.clear();
    let Some(v) = app.selected().cloned() else {
        return;
    };
    let last = area.height.saturating_sub(1);

    let mut x = 1;
    x += put(buf, area, x, 0, "board ", theme::dim());
    put(buf, area, x, 0, &format!("▸ {}", v.task.identifier), theme::fg_default());
    let right_top = match (&v.workspace, &v.pane_id) {
        (Some(w), Some(p)) => format!("ws:{w}   pane {p}"),
        (Some(w), None) => format!("ws:{w}"),
        _ => String::new(),
    };
    put_right(buf, area, area.width.saturating_sub(2), 0, &right_top, theme::dim());
    rule(buf, area, 1, area.width.saturating_sub(2), 1, theme::dim());

    let state = v.state();
    put(buf, area, COL_GUTTER, 3, state.glyph(), theme::state_style(state));
    let state_label = state.as_str();
    let title_max = area
        .width
        .saturating_sub(2)
        .saturating_sub(state_label.chars().count() as u16 + 2)
        .saturating_sub(COL_ID);
    put(
        buf,
        area,
        COL_ID,
        3,
        &truncate(&v.task.title, title_max as usize),
        theme::fg_default(),
    );
    put_right(buf, area, area.width.saturating_sub(2), 3, state_label, theme::dim());

    // Never render full issue bodies — `o` opens the browser, which is what
    // browsers are for.
    let mut y = 5;
    if let Some(body) = &v.task.body {
        for line in wrap(body, area.width.saturating_sub(6) as usize)
            .into_iter()
            .take(4)
        {
            put(buf, area, COL_ID, y, &line, theme::dim());
            y += 1;
        }
        y += 1;
    }

    if state == BoardState::Failed {
        put(
            buf,
            area,
            COL_ID,
            y,
            "pane exited without completing",
            theme::fg_default(),
        );
        y += 2;
    }

    // The one thing detail must say about a reaped row: the attempts below are
    // all that is left of it, and `source` above no longer resolves.
    if v.task.upstream == UpstreamState::Gone {
        put(
            buf,
            area,
            COL_ID,
            y,
            "deleted upstream · kept for the attempts below",
            theme::fg_default(),
        );
        y += 2;
    }

    if !v.task.attempts.is_empty() {
        put(buf, area, 1, y, "attempts", theme::dim());
        y += 1;
        for (i, a) in v.task.attempts.iter().enumerate() {
            if y >= last.saturating_sub(6) {
                break;
            }
            let dur = match (&a.ended_at, &a.started_at) {
                (Some(e), s) => attempt_duration(s, e),
                (None, s) => attempt_duration(s, &crate::db::now()),
            };
            let mut x = COL_ID;
            x += put(buf, area, x, y, &format!("{}  ", i + 1), theme::dim());
            x += put(buf, area, x, y, &fixed(&a.runtime, 13), theme::dim());
            x += put(buf, area, x, y, &fixed(&dur, 9), theme::dim());
            let outcome = a.outcome.map(|o| o.as_str()).unwrap_or("live");
            put(buf, area, x, y, outcome, theme::fg_default());
            y += 1;
        }
        y += 1;
    }

    // Who released this. Default fg when an agent did, because that is the case
    // worth noticing; dim `you` otherwise.
    if y < last - 1 {
        put(buf, area, 1, y, "dispatched by", theme::dim());
        match v.dispatched_by.as_deref() {
            Some(parent) => put(
                buf,
                area,
                15,
                y,
                &format!("{parent}  ·  agent, not you"),
                theme::fg_default(),
            ),
            None => put(buf, area, 15, y, "you", theme::dim()),
        };
        y += 1;
    }

    for (label, value, style) in [
        ("branch", v.branch.clone(), theme::dim()),
        ("pull request", v.task.pr_url.clone(), theme::fg_default()),
        ("source", Some(v.task.url.clone()), theme::dim()),
    ] {
        let Some(value) = value else { continue };
        if y >= last - 1 {
            break;
        }
        put(buf, area, 1, y, label, theme::dim());
        put(buf, area, 15, y, &truncate(&value, area.width.saturating_sub(16) as usize), style);
        y += 1;
    }

    rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
    render_footer(buf, area, last, app);
}

fn attempt_duration(start: &str, end: &str) -> String {
    let (Ok(s), Ok(e)) = (
        chrono::DateTime::parse_from_rfc3339(start),
        chrono::DateTime::parse_from_rfc3339(end),
    ) else {
        return String::new();
    };
    format_elapsed((e - s).num_seconds().max(0))
}

// ---- prompt ------------------------------------------------------------

pub fn render_prompt(buf: &mut Buffer, area: Rect, app: &mut App) {
    app.footer_hits.clear();
    let Some(v) = app.selected().cloned() else {
        return;
    };
    let last = area.height.saturating_sub(1);

    let mut x = 1;
    x += put(
        buf,
        area,
        x,
        0,
        &format!("board ▸ {} ", v.task.identifier),
        theme::dim(),
    );
    put(buf, area, x, 0, "▸ prompt", theme::fg_default());
    if let Some(route) = &v.route_name {
        put_right(
            buf,
            area,
            area.width.saturating_sub(2),
            0,
            &format!("route  {route}"),
            theme::dim(),
        );
    }
    rule(buf, area, 1, area.width.saturating_sub(2), 1, theme::dim());

    // No route → say so plainly, one level deeper than the row hint.
    let Some(prompt) = &v.resolved_prompt else {
        put(
            buf,
            area,
            1,
            3,
            &format!(
                "No route matches {}, so it cannot be dispatched.",
                v.task.identifier
            ),
            theme::fg_default(),
        );
        put(buf, area, 1, 5, &app.config_path, theme::dim());
        rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
        render_footer(buf, area, last, app);
        return;
    };

    // Which attempt: prompts differ between attempts if routing.toml changed in
    // between, so the provenance line names the attempt and its runtime.
    let provenance = match v.task.attempts.last() {
        Some(a) => {
            let by = match v.dispatched_by.as_deref() {
                Some(parent) => format!(" · dispatched by {parent}"),
                None => String::new(),
            };
            format!(
                "as sent, attempt {} · {}{}",
                v.task.attempts.len(),
                a.runtime,
                by
            )
        }
        None => "not dispatched yet — this is what enter would send".to_string(),
    };
    put(buf, area, 1, 3, &provenance, theme::dim());

    for (y, line) in (5..).zip(wrap(prompt, area.width.saturating_sub(6) as usize)) {
        if y >= last.saturating_sub(3) {
            break;
        }
        put(buf, area, COL_ID, y, &line, theme::fg_default());
    }

    put(
        buf,
        area,
        1,
        last.saturating_sub(3),
        &format!(
            "from routing.toml · [[route]] {}",
            v.route_name.as_deref().unwrap_or("")
        ),
        theme::dim(),
    );
    rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
    render_footer(buf, area, last, app);
}

// ---- help --------------------------------------------------------------

pub const HELP_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "move",
        &[
            ("j k ↓ ↑", "move selection"),
            ("enter", "dispatch, or open detail"),
            ("l", "detail view"),
            ("h esc", "back"),
            ("p", "prompt view"),
        ],
    ),
    (
        "dispatch",
        &[
            ("enter", "open the dispatch picker"),
            ("r", "re-dispatch a failed task"),
            ("d", "mark a failed task done"),
            ("x", "cancel — kills the pane"),
            // The footer offers `m merge` on a review row and the key map has
            // always bound it; only this list had never caught up.
            ("m", "merge the pull request"),
        ],
    ),
    (
        "filter",
        &[
            ("f", "show one route, then the next, then all of them"),
            ("/", "find — matches identifier, title and route as you type"),
            ("F", "clear the filter"),
        ],
    ),
    (
        "unadopted",
        &[
            ("a", "adopt — asks what the repo would put on the board"),
            ("space", "pick a label to poll, on that screen"),
            ("enter", "adopt with what is picked; esc writes nothing"),
            ("X", "ignore — a repo you are only reading"),
        ],
    ),
    (
        "elsewhere",
        &[
            ("g", "focus the bound herdr pane"),
            ("o", "open the source in a browser"),
            ("s", "force a sync now"),
            ("t", "throughput — is any of this working"),
            ("?", "this help"),
        ],
    ),
];

/// One drawn line of the key reference: a key and what it does, a group
/// heading (no key), or the blank between groups (neither).
type HelpLine = (&'static str, &'static str);

/// [`HELP_GROUPS`] flattened to the lines it draws, so the screen can be
/// windowed rather than cut off at whatever height the pane happens to be.
pub fn help_lines() -> Vec<HelpLine> {
    let mut out: Vec<HelpLine> = Vec::new();
    for (group, keys) in HELP_GROUPS {
        if !out.is_empty() {
            out.push(("", ""));
        }
        out.push(("", group));
        out.extend(keys.iter().map(|(k, d)| (*k, *d)));
    }
    out
}

/// How many lines of the reference a pane this tall can show.
pub fn help_body_height(height: u16) -> usize {
    // Rows 3..=last-3: the title and its rule above, the `ctrl+b` line, the
    // rule and the footer below.
    height.saturating_sub(1).saturating_sub(5) as usize
}

pub fn render_help(buf: &mut Buffer, area: Rect, app: &mut App) {
    app.footer_hits.clear();
    let last = area.height.saturating_sub(1);
    let mut x = 1;
    x += put(buf, area, x, 0, "board ", theme::dim());
    put(buf, area, x, 0, "▸ keys", theme::fg_default());
    rule(buf, area, 1, area.width.saturating_sub(2), 1, theme::dim());

    // The board binds more keys than a 24-row pane has lines for, and a
    // reference that silently stops halfway is how `m` stayed undocumented: it
    // scrolls, and says that it does.
    let lines = help_lines();
    let height = help_body_height(area.height);
    app.help_scroll = app.help_scroll.min(lines.len().saturating_sub(height));

    for (y, (key, desc)) in (3..).zip(lines.iter().skip(app.help_scroll).take(height)) {
        match (key.is_empty(), desc.is_empty()) {
            (true, true) => {}
            (true, false) => {
                put(
                    buf,
                    area,
                    1,
                    y,
                    desc,
                    theme::dim().add_modifier(Modifier::BOLD),
                );
            }
            _ => {
                put(buf, area, COL_ID, y, &fixed(key, 9), theme::fg_default());
                put(buf, area, COL_TITLE, y, desc, theme::dim());
            }
        }
    }

    put(
        buf,
        area,
        1,
        last.saturating_sub(2),
        "ctrl+b reaches herdr from here — the board never swallows the prefix.",
        theme::dim(),
    );
    rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
    // The same marker the list uses, in the same place, for the same reason.
    if let Some(marker) = scroll_marker(lines.len(), app.help_scroll, height) {
        put_right(buf, area, area.width.saturating_sub(2), last - 1, &marker, theme::dim());
    }
    render_footer(buf, area, last, app);
}

/// `↑3 ↓12` for a window onto a longer list, or `None` when it all fits.
///
/// A screen that scrolls has to say so, or a short pane looks like the whole of
/// what there is.
pub fn scroll_marker(len: usize, scroll: usize, height: usize) -> Option<String> {
    if len <= height {
        return None;
    }
    let more = len - scroll - height.min(len - scroll);
    Some(if scroll > 0 && more > 0 {
        format!(" ↑{scroll} ↓{more} ")
    } else if scroll > 0 {
        format!(" ↑{scroll} ")
    } else {
        format!(" ↓{more} ")
    })
}

/// Keep the selected row inside the window, scrolling as little as possible.
pub fn clamp_scroll(
    lines: &[Row],
    selected: Option<&str>,
    scroll: usize,
    height: usize,
) -> usize {
    if height == 0 || lines.len() <= height {
        return 0;
    }
    let max = lines.len() - height;
    let Some(id) = selected else {
        return scroll.min(max);
    };
    // Headers are selectable rows that are not tasks, so they have to be
    // findable here too or selecting one below the fold scrolls nowhere.
    let Some(ix) = lines.iter().position(|r| r.id() == id) else {
        return scroll.min(max);
    };
    // Keep the section header above the selection visible where it is cheap to
    // do so — a row with no header above it loses the state it is in.
    let want_top = ix.saturating_sub(1);
    let scroll = if want_top < scroll {
        want_top
    } else if ix >= scroll + height {
        ix + 1 - height
    } else {
        scroll
    };
    scroll.min(max)
}

/// Word-wrap to `width`, preserving existing hard breaks.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
        }
        out.push(line);
    }
    out
}

pub fn render(buf: &mut Buffer, area: Rect, app: &mut App) {
    // Recorded here rather than only by the event loop, so anything that draws
    // a frame gets the footer's click row and the height-dependent hints right
    // without having to remember to set it.
    app.last_height = area.height;
    match app.screen {
        Screen::List => render_list(buf, area, app),
        Screen::Detail => render_detail(buf, area, app),
        Screen::Prompt => render_prompt(buf, area, app),
        Screen::Help => render_help(buf, area, app),
        Screen::Stats => render_stats(buf, area, app),
        Screen::Adopt => render_adopt(buf, area, app),
    }
}

// ---- adopt -------------------------------------------------------------

/// Picked and not picked. A filled/hollow pair rather than colour, so the
/// distinction survives `NO_COLOR` and a monochrome terminal.
pub const PICKED: &str = "●";
pub const UNPICKED: &str = "○";

/// What adopting a repo would put on the board, before it does.
///
/// The whole screen is one question — "83 rows, or which of these?" — so it
/// answers it in the first two lines and spends the rest on the labels.
pub fn render_adopt(buf: &mut Buffer, area: Rect, app: &mut App) {
    app.footer_hits.clear();
    app.adopt_hits.clear();
    let last = area.height.saturating_sub(1);
    let right = area.width.saturating_sub(2);
    let Some(view) = app.adopt.clone() else {
        rule(buf, area, 1, right, last.saturating_sub(1), theme::dim());
        render_footer(buf, area, last, app);
        return;
    };

    let mut x = 1;
    x += put(buf, area, x, 0, "board ", theme::dim());
    put(
        buf,
        area,
        x,
        0,
        &format!("▸ adopt {}", view.repo.slug),
        theme::fg_default(),
    );
    let scale = match &view.preview {
        Some(Ok(p)) => p.count_phrase(),
        Some(Err(_)) => "github could not be asked".to_string(),
        None => "asking github…".to_string(),
    };
    put_right(buf, area, right, 0, &scale, theme::dim());
    rule(buf, area, 1, right, 1, theme::dim());

    // Line one says what happens if you just press enter, because that is what
    // most adoptions will do and it is the outcome that surprised somebody.
    let opening = match &view.preview {
        None => format!(
            "Asking GitHub how many open issues {} holds, and what they are \
             labelled.",
            view.repo.slug
        ),
        Some(Ok(p)) if p.open_issues == 0 => {
            "No open issues. Adopting costs nothing; rows arrive when issues do."
                .to_string()
        }
        Some(Ok(p)) if p.labels.is_empty() => format!(
            "{} would become rows. Its issues carry no labels, so there is \
             nothing to narrow it by here.",
            p.count_phrase()
        ),
        Some(Ok(p)) => format!(
            "{} would become rows. A board is a queue of work in play, and a \
             repo's whole backlog is not that — pick the labels that are current.",
            p.count_phrase()
        ),
        Some(Err(e)) => format!(
            "Could not ask GitHub what this repo holds: {e}. Adopting still \
             works; it polls whatever `[github] labels` lets through."
        ),
    };
    let mut y = 3;
    for line in wrap(&opening, right.saturating_sub(1) as usize) {
        if y >= last.saturating_sub(3) {
            break;
        }
        put(buf, area, 1, y, &line, theme::dim());
        y += 1;
    }
    y += 1;

    let rows = view.label_rows();
    if !rows.is_empty() && y < last.saturating_sub(3) {
        put(buf, area, 1, y, "poll only issues labelled", theme::dim());
        // The number that moves as you pick: this is the board you would get.
        if let Some(n) = view.would_pull() {
            put_right(
                buf,
                area,
                right,
                y,
                &format!("{n} row{}", if n == 1 { "" } else { "s" }),
                theme::fg_default(),
            );
        }
        y += 1;

        // Windowed on the cursor: a repo can carry more labels than a short
        // pane has lines, and a pick you cannot scroll to is not offered.
        let bottom = last.saturating_sub(3);
        let height = bottom.saturating_sub(y) as usize;
        let start = adopt_scroll(view.cursor, rows.len(), height);
        for (ix, (label, count)) in rows.iter().enumerate().skip(start).take(height) {
            put(
                buf,
                area,
                COL_GUTTER,
                y,
                if view.is_chosen(ix) { PICKED } else { UNPICKED },
                if view.is_chosen(ix) {
                    theme::fg_default()
                } else {
                    theme::dim()
                },
            );
            let width = right.saturating_sub(COL_ID + 6);
            put(
                buf,
                area,
                COL_ID,
                y,
                &truncate(label, width as usize),
                theme::fg_default(),
            );
            put_right(buf, area, right, y, &count.to_string(), theme::dim());
            if ix == view.cursor {
                reverse_row(buf, area, y, COL_GUTTER);
            }
            app.adopt_hits.push((y, ix));
            y += 1;
        }
        if rows.len() > start + height {
            put_right(
                buf,
                area,
                right,
                bottom,
                &format!("{} more ↓", rows.len() - start - height),
                theme::dim(),
            );
        }
    }

    // What the choice means in the file, since the file is what it becomes.
    let note = match view.written_labels() {
        Some(labels) => format!(
            "writes `[[github.repo]] labels = [{}]` · delete it later to widen",
            labels
                .iter()
                .map(|l| format!("\"{l}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        // Picking nothing leaves the repo on the global list — which is what
        // every adoption did before this screen, and is right whenever the
        // repo's tracker is already curated.
        None if view.fallback.is_empty() => {
            "nothing picked · every open issue, as `[github] labels = []` already says"
                .to_string()
        }
        None => format!(
            "nothing picked · falls back to `[github] labels = [{}]`",
            view.fallback
                .iter()
                .map(|l| format!("\"{l}\""))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    put(
        buf,
        area,
        1,
        last.saturating_sub(2),
        &truncate(&note, right as usize),
        theme::dim(),
    );
    rule(buf, area, 1, right, last.saturating_sub(1), theme::dim());
    render_footer(buf, area, last, app);
}

/// Keep the cursor inside a window of `height` label rows.
pub fn adopt_scroll(cursor: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    cursor.saturating_sub(height - 1).min(len - height)
}

/// Throughput, in the same plain language as everything else. No charts: the
/// design forbids them, and six numbers do not need one.
pub fn render_stats(buf: &mut Buffer, area: Rect, app: &mut App) {
    app.footer_hits.clear();
    let last = area.height.saturating_sub(1);
    let mut x = 1;
    x += put(buf, area, x, 0, "board ", theme::dim());
    put(buf, area, x, 0, "▸ stats", theme::fg_default());

    let Some(s) = app.stats.clone() else {
        put(buf, area, 1, 3, "no dispatches yet", theme::dim());
        rule(buf, area, 1, area.width.saturating_sub(2), 1, theme::dim());
        rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
        render_footer(buf, area, last, app);
        return;
    };
    let window = match s.since_days {
        Some(d) => format!("last {d} days"),
        None => "all time".to_string(),
    };
    put_right(buf, area, area.width.saturating_sub(2), 0, &window, theme::dim());
    rule(buf, area, 1, area.width.saturating_sub(2), 1, theme::dim());

    if s.attempts == 0 {
        put(buf, area, 1, 3, "no dispatches yet", theme::dim());
        rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
        render_footer(buf, area, last, app);
        return;
    }

    put(
        buf,
        area,
        1,
        3,
        &format!(
            "{} dispatches across {} task{}",
            s.attempts,
            s.tasks_touched,
            if s.tasks_touched == 1 { "" } else { "s" }
        ),
        theme::fg_default(),
    );

    let mut rows: Vec<(String, String)> = Vec::new();
    if s.live > 0 {
        rows.push(("running".into(), format!("{}", s.live)));
    }
    let outcomes: Vec<String> = s.outcomes.iter().map(|(k, v)| format!("{v} {k}")).collect();
    if !outcomes.is_empty() {
        rows.push(("finished".into(), outcomes.join(" · ")));
    }
    if let Some(rate) = s.completion_rate {
        rows.push(("completed".into(), format!("{:.0}%", rate * 100.0)));
    }
    if let (Some(med), Some(max)) = (s.median_minutes, s.longest_minutes) {
        rows.push((
            "duration".into(),
            format!("{med}m median · {max}m longest"),
        ));
    }
    if s.retried_tasks > 0 {
        rows.push((
            "retried".into(),
            format!(
                "{} task{} needed more than one go",
                s.retried_tasks,
                if s.retried_tasks == 1 { "" } else { "s" }
            ),
        ));
    }
    // The number that says whether the herd is releasing its own work.
    rows.push((
        "released".into(),
        if s.agent_dispatched > 0 {
            format!("{} by an agent, the rest by you", s.agent_dispatched)
        } else {
            "all by you".to_string()
        },
    ));

    let tally = |m: &std::collections::BTreeMap<String, usize>| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        v.iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    if !s.by_workspace.is_empty() {
        rows.push(("workspace".into(), tally(&s.by_workspace)));
    }
    if !s.by_runtime.is_empty() {
        rows.push(("runtime".into(), tally(&s.by_runtime)));
    }

    for (y, (label, value)) in (5..).zip(rows) {
        if y >= last - 1 {
            break;
        }
        put(buf, area, 1, y, &label, theme::dim());
        put(
            buf,
            area,
            15,
            y,
            &truncate(&value, area.width.saturating_sub(16) as usize),
            theme::fg_default(),
        );
    }

    rule(buf, area, 1, area.width.saturating_sub(2), last - 1, theme::dim());
    render_footer(buf, area, last, app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::fixtures;

    fn draw(app: &mut App, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, app);
        buf
    }

    fn line(buf: &Buffer, y: u16) -> String {
        let w = buf.area.width;
        (0..w)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn the_board_draws_no_vertical_rules() {
        // The design's hardest rule: herdr owns pane chrome. A `│` here gives
        // the operator doubled dividers.
        for scenario in fixtures::ALL {
            let mut app = fixtures::app(scenario);
            for (w, h) in [(80u16, 24u16), (120, 40), (160, 38)] {
                for screen in [Screen::List, Screen::Detail, Screen::Prompt, Screen::Help] {
                    app.screen = screen;
                    let buf = draw(&mut app, w, h);
                    for y in 0..h {
                        let l = line(&buf, y);
                        assert!(
                            !l.contains('│') && !l.contains('|') && !l.contains('┌'),
                            "{scenario} {screen:?} row {y} drew pane chrome: {l:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn nothing_paints_a_background() {
        for scenario in fixtures::ALL {
            let mut app = fixtures::app(scenario);
            let buf = draw(&mut app, 80, 24);
            for y in 0..24 {
                for x in 0..80 {
                    assert_eq!(
                        buf[(x, y)].bg,
                        Color::Reset,
                        "{scenario} cell ({x},{y}) painted a background"
                    );
                }
            }
        }
    }

    #[test]
    fn columns_are_where_the_design_says() {
        let mut app = fixtures::app(fixtures::POPULATED);
        let buf = draw(&mut app, 80, 24);
        // Find a task row and check the grid: gutter at 1, id at 3, title at 13.
        let y = app
            .rows
            .iter()
            .find(|(_, r)| matches!(r, Row::Task(_)))
            .unwrap()
            .0;
        let l = line(&buf, y);
        assert_eq!(l.chars().next().unwrap(), ' ', "col 0 is a blank margin");
        assert_ne!(l.chars().nth(1).unwrap(), ' ', "gutter glyph at col 1");
        assert_eq!(l.chars().nth(2).unwrap(), ' ');
        assert_ne!(l.chars().nth(3).unwrap(), ' ', "id starts at col 3");
        assert_ne!(l.chars().nth(13).unwrap(), ' ', "title starts at col 13");
    }

    #[test]
    fn an_unadopted_repo_is_named_on_the_board_rather_than_being_silent() {
        // The bug: a repo with a herdr workspace and no board config polls
        // nothing and says nothing. The section is the whole fix.
        let mut app = fixtures::app(fixtures::UNADOPTED);
        let buf = draw(&mut app, 80, 24);
        let body: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        assert!(
            body.iter().any(|l| l.contains("UNADOPTED")),
            "no section header:\n{}",
            body.join("\n")
        );
        let row = body
            .iter()
            .find(|l| l.contains("tripletex-mcp"))
            .expect("the repo is not on screen");
        assert!(
            row.contains("Florin-AS/tripletex-mcp"),
            "the slug is what gets written to config, so it has to be shown: {row:?}"
        );
        // The half-fixes say which half, or the operator cannot tell a row that
        // will not dispatch from one that is never polled. They also say what
        // *is* configured: a bare "no route" on a polled repo reads as "this
        // workspace cannot be dispatched to", which is false whenever its
        // tickets live in Linear and route by label.
        assert!(
            body.iter().any(|l| l.contains("polled, no route")),
            "{body:?}"
        );
        assert!(
            body.iter().any(|l| l.contains("routed, not polled")),
            "{body:?}"
        );
    }

    // ---- AGE-28: the adoption screen -----------------------------------

    fn adopting() -> App {
        let mut app = fixtures::app(fixtures::UNADOPTED);
        let repo = app.unadopted[0].clone();
        app.adopt = Some(AdoptView::new(
            repo,
            Some(Ok(fixtures::repo_preview())),
            Vec::new(),
        ));
        app.screen = Screen::Adopt;
        app
    }

    #[test]
    fn adopting_says_how_much_of_the_board_the_repo_would_become() {
        // The bug: adoption wrote `[github] repos` with no filter and 83 rows
        // arrived on the next poll — 76% of everything not done — with nothing
        // having said so beforehand.
        let mut app = adopting();
        let buf = draw(&mut app, 80, 24);
        let body: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        let all = body.join("\n");
        assert!(all.contains("83 open issues"), "{all}");
        assert!(
            all.contains("Florin-AS/tripletex-mcp"),
            "the repo being decided about is not named:\n{all}"
        );
        // And the labels that would narrow it, largest first.
        let a = all.find("release-a").expect("no labels offered");
        let b = all.find("release-b").expect("no labels offered");
        assert!(a < b, "labels are not commonest-first:\n{all}");
        assert!(all.contains("68"), "counts are what make a label worth picking");
    }

    #[test]
    fn the_row_count_tracks_what_is_picked() {
        let mut app = adopting();
        let before = (0..24)
            .map(|y| line(&draw(&mut app, 80, 24), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(before.contains("83 rows"), "{before}");

        // Pick `release-a`, which the fixture says 68 issues carry.
        app.adopt.as_mut().unwrap().toggle();
        let after = (0..24)
            .map(|y| line(&draw(&mut app, 80, 24), y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(after.contains("68 rows"), "{after}");
        // ...and it says what that becomes in the file, since the file is what
        // it becomes.
        assert!(
            after.contains("[[github.repo]] labels = [\"release-a\"]"),
            "{after}"
        );
    }

    #[test]
    fn a_repo_github_could_not_be_asked_about_is_still_adoptable() {
        // The preview is a courtesy, not a gate: a 404, a missing token or a
        // rate limit must not make a repo un-adoptable.
        let mut app = adopting();
        app.adopt.as_mut().unwrap().preview = Some(Err("github HTTP 404".into()));
        let buf = draw(&mut app, 80, 24);
        let body: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        let all = body.join("\n");
        // Wrapped across rows, so the prose is checked unwrapped.
        let flat = body.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("404"), "the reason is not shown:\n{all}");
        assert!(
            flat.contains("Adopting still works"),
            "it reads as a dead end:\n{all}"
        );
        assert!(all.contains("enter"), "no way forward is offered:\n{all}");
    }

    #[test]
    fn the_wait_for_github_is_drawn_rather_than_frozen() {
        // The screen opens before the answer arrives, and the board blocks on
        // that call — so the frame in between has to say what it is waiting on
        // rather than looking hung.
        let mut app = adopting();
        app.adopt.as_mut().unwrap().preview = None;
        let buf = draw(&mut app, 80, 24);
        let body: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        let flat = body.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("asking github"), "{flat}");
        assert!(
            flat.contains("Asking GitHub how many open issues"),
            "it does not say what it is waiting for:\n{}",
            body.join("\n")
        );
        // Nothing to pick yet, so nothing is offered to pick.
        assert!(app.adopt_hits.is_empty());
    }

    #[test]
    fn the_screen_offers_a_way_out_that_writes_nothing() {
        // Every other confirmation on this board can be declined, and this one
        // is the only screen that appears on the way to a file write.
        let mut app = adopting();
        let buf = draw(&mut app, 80, 24);
        let footer = line(&buf, 23);
        assert!(footer.contains("esc"), "{footer}");
        assert!(footer.contains("nothing is written"), "{footer}");
    }

    #[test]
    fn more_labels_than_fit_are_reachable_rather_than_clipped() {
        // A pick you cannot scroll to is a pick that is not offered.
        let mut app = adopting();
        let many: Vec<(String, usize)> =
            (0..40).map(|i| (format!("label-{i:02}"), 40 - i)).collect();
        app.adopt.as_mut().unwrap().preview = Some(Ok(crate::adopt::RepoPreview {
            open_issues: 83,
            truncated: false,
            labels: many,
        }));
        for _ in 0..39 {
            app.adopt.as_mut().unwrap().move_cursor(1);
        }
        let buf = draw(&mut app, 80, 24);
        let all: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        assert!(
            all.iter().any(|l| l.contains("label-39")),
            "the last label is unreachable:\n{}",
            all.join("\n")
        );
        // And the cursor did not run past the end.
        assert_eq!(app.adopt.as_ref().unwrap().cursor, 39);
    }

    #[test]
    fn the_adoption_screen_obeys_the_boards_rules_at_every_size() {
        // The same three the rest of the board is held to: herdr owns pane
        // chrome, nothing paints a background, and every row is exactly as
        // wide as the pane.
        for (w, h) in [(80u16, 24u16), (120, 40), (160, 38), (60, 20), (60, 8)] {
            let mut app = adopting();
            let buf = draw(&mut app, w, h);
            for y in 0..h {
                let l: String = (0..w)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect();
                assert_eq!(l.chars().count() as u16, w, "{w}x{h} row {y}");
                assert!(
                    !l.contains('│') && !l.contains('|') && !l.contains('┌'),
                    "{w}x{h} row {y} drew pane chrome: {l:?}"
                );
                for x in 0..w {
                    assert_eq!(buf[(x, y)].bg, Color::Reset, "{w}x{h} ({x},{y})");
                }
            }
        }
    }

    #[test]
    fn a_label_row_is_clickable() {
        // herdr is mouse-first, and a picker you cannot click is half a picker.
        let mut app = adopting();
        let _ = draw(&mut app, 80, 24);
        let (y, ix) = app.adopt_hits[1];
        let mut last_click = None;
        let click = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 10,
            row: y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(
            super::super::app::mouse_action(&mut app, click, &mut last_click),
            super::super::app::Action::Toggle
        );
        assert_eq!(app.adopt.as_ref().unwrap().cursor, ix);
    }

    #[test]
    fn the_unadopted_header_never_claims_more_than_its_rows() {
        use crate::adopt::Missing;
        // Seen live: the header read "not polled, not dispatchable" above a row
        // for a repo that *was* polled and merely lacked a `gh_repo` route.
        assert_eq!(
            Missing::header_note(&[Missing::Route]),
            "no route for their issues  ",
            "a polled repo must not be described as unpolled"
        );
        assert_eq!(
            Missing::header_note(&[Missing::Polling]),
            "not polled  ",
            "a routed repo must not be described as undispatchable"
        );
        // Only what every row shares.
        assert_eq!(
            Missing::header_note(&[Missing::Route, Missing::Polling]),
            "missing config  "
        );
        assert_eq!(
            Missing::header_note(&[Missing::Both, Missing::Both]),
            "not polled, not dispatchable  "
        );
    }

    #[test]
    fn an_empty_queue_still_draws_the_reason_it_is_empty() {
        // "Nothing queued" and "and here is the repo nobody adopted" have to
        // share a body; showing only the first is the silence AGE-18 removes.
        let mut app = fixtures::app(fixtures::UNADOPTED);
        assert!(app.is_empty());
        let buf = draw(&mut app, 80, 24);
        let body: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        assert!(body.iter().any(|l| l.contains("Nothing queued")));
        assert!(body.iter().any(|l| l.contains("UNADOPTED")));
    }

    #[test]
    fn the_unadopted_glyph_is_not_any_states_glyph() {
        // Rule 3: color gets stripped, so the shape is the only carrier left.
        for st in BoardState::SECTION_ORDER {
            assert_ne!(st.glyph(), UNADOPTED_GLYPH, "{st} shares the adoption glyph");
        }
    }

    #[test]
    fn the_section_draws_last_however_full_the_board_is() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.collapsed.clear();
        let buf = draw(&mut app, 80, 40);
        let body: Vec<String> = (0..40).map(|y| line(&buf, y)).collect();
        let unadopted = body.iter().position(|l| l.contains("UNADOPTED")).unwrap();
        let done = body.iter().position(|l| l.contains("DONE today")).unwrap();
        assert!(unadopted > done, "it pushed the queue down:\n{}", body.join("\n"));
    }

    #[test]
    fn a_narrow_pane_keeps_the_name_and_drops_the_slug() {
        // A truncated `owner/repo` identifies nothing; the workspace label is
        // the part you recognise.
        let mut app = fixtures::app(fixtures::UNADOPTED);
        let buf = draw(&mut app, 44, 24);
        let body: Vec<String> = (0..24).map(|y| line(&buf, y)).collect();
        assert!(body.iter().any(|l| l.contains("tripletex-mcp")));
        assert!(
            !body.iter().any(|l| l.contains("Florin-AS/")),
            "a half-slug on a narrow pane:\n{}",
            body.join("\n")
        );
    }

    #[test]
    fn the_selection_is_always_on_screen() {
        // Without a viewport the body is just clipped, and every row below the
        // fold is invisible and unreachable.
        let mut app = fixtures::app(fixtures::POPULATED);
        app.collapsed.clear();
        let ids = app.visible_task_ids();
        // A pane short enough that most of the board is below the fold.
        for id in &ids {
            app.selected_id = Some(id.clone());
            let buf = draw(&mut app, 80, 12);
            let on_screen = app.rows.iter().any(|(_, r)| &r.id() == id);
            assert!(on_screen, "{id:?} was selected but never drawn");
            let _ = buf;
        }
    }

    #[test]
    fn scrolling_stops_at_both_ends() {
        let lines: Vec<Row> = (0..10)
            .map(|i| Row::Task(format!("t{i}")))
            .collect();
        // Never scrolls past the end.
        assert_eq!(clamp_scroll(&lines, Some("t9"), 0, 4), 6);
        // Never scrolls above the top.
        assert_eq!(clamp_scroll(&lines, Some("t0"), 6, 4), 0);
        // A list that fits never scrolls at all.
        assert_eq!(clamp_scroll(&lines, Some("t9"), 3, 20), 0);
    }

    #[test]
    fn a_selection_already_on_screen_does_not_move_the_view() {
        let lines: Vec<Row> = (0..10).map(|i| Row::Task(format!("t{i}"))).collect();
        // t5 is visible in the window [4..8), so the view stays put.
        assert_eq!(clamp_scroll(&lines, Some("t5"), 4, 4), 4);
    }

    #[test]
    fn a_short_pane_says_there_is_more_below() {
        let mut app = fixtures::app(fixtures::POPULATED);
        let buf = draw(&mut app, 80, 10);
        let rule = line(&buf, 8);
        assert!(rule.contains('↓'), "no more-below marker: {rule:?}");
    }

    #[test]
    fn sections_render_in_fixed_order() {
        let mut app = fixtures::app(fixtures::POPULATED);
        draw(&mut app, 80, 24);
        let order: Vec<BoardState> = app
            .rows
            .iter()
            .filter_map(|(_, r)| match r {
                Row::Section(s) => Some(*s),
                _ => None,
            })
            .collect();
        let mut sorted = order.clone();
        sorted.sort_by_key(|s| {
            BoardState::SECTION_ORDER
                .iter()
                .position(|x| x == s)
                .unwrap()
        });
        assert_eq!(order, sorted, "sections must follow the fixed order");
    }

    #[test]
    fn the_selected_row_is_one_solid_reversed_bar() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.selected_id = app.first_task();
        let buf = draw(&mut app, 80, 24);
        let y = app
            .rows
            .iter()
            .find(|(_, r)| matches!(r, Row::Task(id) if Some(id.as_str()) == app.selected_id.as_deref()))
            .unwrap()
            .0;
        for x in 0..80 {
            let c = &buf[(x, y)];
            assert!(
                c.modifier.contains(Modifier::REVERSED),
                "cell {x} of the selected row is not reversed"
            );
            assert!(
                !c.modifier.contains(Modifier::DIM),
                "cell {x} kept DIM, which mottles the bar"
            );
        }
        // ...except the gutter, which keeps its state color as a cursor block.
        assert_ne!(buf[(COL_GUTTER, y)].fg, Color::Reset);
    }

    #[test]
    fn a_narrow_header_never_writes_over_the_title() {
        // A split board can be far narrower than 80 cells.
        for w in [30u16, 42, 50, 60, 80] {
            let mut app = fixtures::app(fixtures::POPULATED);
            let buf = draw(&mut app, w, 24);
            let l = line(&buf, 0);
            assert!(
                l.starts_with(" herdr-board") || w < 14,
                "at {w} cols the header title was overwritten: {l:?}"
            );
        }
    }

    #[test]
    fn header_reports_the_three_distinct_states() {
        let healthy = SyncStatus {
            linear: SourceHealth::Ok,
            github: SourceHealth::Ok,
            last_cycle_secs: Some(12),
            last_source_ok_secs: Some(12),
            interval_secs: 30,
        };
        assert_eq!(header_status(&healthy), "linear ✓   gh ✓   synced 12s");

        // The daemon is alive and cycling; only Linear has gone stale.
        let degraded = SyncStatus {
            linear: SourceHealth::Down {
                error: "refused".into(),
                retry_in: 30,
            },
            last_source_ok_secs: Some(240),
            ..healthy.clone()
        };
        assert_eq!(
            header_status(&degraded),
            "linear ✗ retrying 30s   gh ✓   last synced 4m"
        );

        // A dead daemon is a third state, not a down source.
        let dead = SyncStatus {
            last_cycle_secs: Some(240),
            ..healthy.clone()
        };
        assert_eq!(header_status(&dead), "syncd not running   sync stale 4m");
        assert!(!header_status(&dead).contains("linear ✗"));
    }

    #[test]
    fn a_broken_header_is_not_drawn_as_chrome() {
        let healthy = SyncStatus {
            linear: SourceHealth::Ok,
            github: SourceHealth::Ok,
            last_cycle_secs: Some(12),
            last_source_ok_secs: Some(12),
            interval_secs: 30,
        };
        assert!(header_style(&healthy).add_modifier.contains(Modifier::DIM));

        // Both broken states have to shout on a light terminal, where dim
        // collapses towards the background, and on a monochrome one, where the
        // colour is gone — so: not dim, coloured, and bold.
        let source_down = SyncStatus {
            linear: SourceHealth::Down {
                error: "refused".into(),
                retry_in: 30,
            },
            ..healthy.clone()
        };
        let daemon_dead = SyncStatus {
            last_cycle_secs: Some(240),
            ..healthy.clone()
        };
        for s in [&source_down, &daemon_dead] {
            let style = header_style(s);
            assert!(!style.add_modifier.contains(Modifier::DIM));
            assert!(style.add_modifier.contains(Modifier::BOLD));
            assert_eq!(style.fg, Some(Color::Red));
            // Rule 2 holds here too.
            assert_eq!(style.bg, None);
        }
    }

    #[test]
    fn with_no_source_configured_the_header_still_shows_an_age() {
        let s = SyncStatus {
            linear: SourceHealth::Absent,
            github: SourceHealth::Absent,
            last_cycle_secs: Some(2),
            last_source_ok_secs: None,
            interval_secs: 30,
        };
        assert_eq!(header_status(&s), "synced 2s");
    }

    #[test]
    fn an_unconfigured_source_is_left_out_of_the_header() {
        let s = SyncStatus {
            linear: SourceHealth::Ok,
            github: SourceHealth::Absent,
            last_cycle_secs: Some(3),
            last_source_ok_secs: Some(3),
            interval_secs: 30,
        };
        assert_eq!(header_status(&s), "linear ✓   synced 3s");
    }

    #[test]
    fn a_ready_row_says_where_it_would_go() {
        // `gh#507` does not say which repo, and a Linear row has no repo at
        // all — the routed workspace answers both.
        let mut v = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Ready && v.has_route)
            .cloned()
            .unwrap();
        v.workspace = Some("tally".into());
        v.repo = None;
        assert_eq!(metadata(&v, false, 80), "tally");
        assert_eq!(metadata(&v, true, 80), "tally · [enter to dispatch]");

        // A GitHub row falls back to its repo if it somehow has no route.
        v.workspace = None;
        v.repo = Some("Tally".into());
        assert_eq!(metadata(&v, false, 80), "Tally");

        // Nothing to say, nothing said.
        v.repo = None;
        assert_eq!(metadata(&v, false, 80), "");
    }

    #[test]
    fn no_route_shows_on_every_such_row_but_the_dispatch_hint_only_on_selection() {
        let mut app = fixtures::app(fixtures::POPULATED);
        let unrouted = app
            .views
            .iter()
            .find(|v| !v.has_route)
            .cloned()
            .expect("fixture needs a no-route row");
        assert_eq!(metadata(&unrouted, false, 80), "no route");
        assert_eq!(metadata(&unrouted, true, 80), "no route");

        let ready = app
            .views
            .iter()
            .find(|v| v.state() == BoardState::Ready && v.has_route)
            .cloned()
            .unwrap();
        // The workspace is on every ready row now; the dispatch hint is still
        // only on the selected one.
        let ws = ready.workspace.clone().unwrap();
        assert_eq!(metadata(&ready, false, 80), ws);
        assert_eq!(
            metadata(&ready, true, 80),
            format!("{ws} · [enter to dispatch]")
        );
        app.screen = Screen::List;
    }

    /// AGE-6: a reaped row sits in `done` beside rows that were properly closed,
    /// and the two are there for different reasons — this one because the issue
    /// went away, not because the work landed.
    #[test]
    fn a_row_whose_issue_was_deleted_says_so_where_a_done_row_shows_its_workspace() {
        let done = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Done)
            .cloned()
            .expect("fixture needs a done row");
        assert!(
            !metadata(&done, false, 80).contains("gone upstream"),
            "an ordinary done row must not claim to be gone"
        );

        let mut gone = done;
        gone.task.upstream = UpstreamState::Gone;
        assert!(metadata(&gone, false, 80).contains("gone upstream"));
        // Selection changes nothing: there is no dispatch to hint at.
        assert!(metadata(&gone, true, 80).contains("gone upstream"));
    }

    #[test]
    fn an_idle_working_row_swaps_the_counter_for_a_marker() {
        let mut v = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Working)
            .cloned()
            .unwrap();
        v.elapsed_secs = Some(666);
        v.idle = false;
        let ticking = metadata(&v, false, 80);
        assert!(ticking.ends_with("11m06s"), "{ticking:?}");
        v.idle = true;
        let idle = metadata(&v, false, 80);
        assert!(idle.contains("idle 11m06s"), "{idle:?}");
        // No new state and no new glyph — it is still `working`.
        assert_eq!(v.state(), BoardState::Working);
    }

    fn agent_dispatched_row() -> TaskView {
        fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.dispatched_by.is_some())
            .cloned()
            .expect("the fixture needs an agent-dispatched row")
    }

    fn operator_row() -> TaskView {
        fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Working && v.dispatched_by.is_none() && !v.idle)
            .cloned()
            .unwrap()
    }

    #[test]
    fn provenance_takes_the_runtime_column_at_ordinary_widths() {
        // "Zero title width lost": the agent-dispatched row's metadata block is
        // exactly as wide as an operator row's, with `via` where the runtime was.
        let agent = agent_dispatched_row();
        let operator = operator_row();
        let a = metadata(&agent, false, 80);
        let o = metadata(&operator, false, 80);
        assert_eq!(
            a.chars().count(),
            o.chars().count(),
            "agent row {a:?} is not the same width as operator row {o:?}"
        );
        assert!(a.starts_with("via LIN-138"), "{a:?}");
        assert!(
            !a.contains("codex"),
            "the runtime it displaced must not also appear: {a:?}"
        );
    }

    #[test]
    fn a_wide_board_shows_both_runtime_and_provenance() {
        let a = metadata(&agent_dispatched_row(), false, 120);
        assert!(a.contains("codex"), "runtime returns at 100+ columns: {a:?}");
        assert!(a.contains("via LIN-138"), "{a:?}");
        assert!(a.contains("ws:offhand"), "{a:?}");
    }

    /// The row released by an orchestrator — a pane the board never dispatched,
    /// which is how most agent dispatch actually happens. There is no issue to
    /// name it by, so the pane is the name, in the same column.
    #[test]
    fn a_row_released_by_an_orchestrator_names_its_pane() {
        let v = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.dispatched_by.as_deref() == Some("w1:p3"))
            .cloned()
            .expect("the fixture needs an orchestrator-dispatched row");
        for w in [80, 120] {
            let m = metadata(&v, false, w);
            assert!(m.contains("via w1:p3"), "{m:?}");
        }
    }

    /// Detail says the same thing it says for a parent it can name by issue:
    /// an agent released this, not you.
    #[test]
    fn the_detail_view_names_an_orchestrator_by_its_pane() {
        let mut app = fixtures::app(fixtures::POPULATED);
        let id = app
            .views
            .iter()
            .find(|v| v.dispatched_by.as_deref() == Some("w1:p3"))
            .map(|v| v.id().to_string())
            .unwrap();
        app.screen = Screen::Detail;
        app.selected_id = Some(id);
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("w1:p3  ·  agent, not you"), "{all}");
    }

    #[test]
    fn an_operator_row_never_mentions_provenance() {
        // Only the case worth noticing is called out; `you` is the default and
        // stays out of the list entirely.
        for w in [80, 120] {
            let o = metadata(&operator_row(), false, w);
            assert!(!o.contains("via"), "{o:?}");
            assert!(o.contains("claude-code"), "{o:?}");
        }
    }

    #[test]
    fn provenance_survives_narrowing_longer_than_the_runtime_did() {
        let agent = agent_dispatched_row();
        // At the narrow end everything goes, as for any row.
        assert!(metadata(&agent, false, 59).is_empty());
    }

    #[test]
    fn the_detail_view_names_who_released_the_task() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Detail;
        app.selected_id = Some(agent_dispatched_row().id().to_string());
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("dispatched by"), "{all}");
        assert!(all.contains("LIN-138  ·  agent, not you"), "{all}");
    }

    #[test]
    fn the_detail_view_says_you_for_an_operator_dispatch() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Detail;
        app.selected_id = Some(operator_row().id().to_string());
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("dispatched by"), "{all}");
        assert!(!all.contains("agent, not you"), "{all}");
    }

    #[test]
    fn the_prompt_view_appends_provenance_to_its_line() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Prompt;
        app.selected_id = Some(agent_dispatched_row().id().to_string());
        let buf = draw(&mut app, 80, 24);
        assert!(
            line(&buf, 3).contains("dispatched by LIN-138"),
            "{:?}",
            line(&buf, 3)
        );
        assert!(line(&buf, 3).starts_with(" as sent, attempt 1 · codex"));
    }

    #[test]
    fn metadata_collapses_right_to_left_and_vanishes_below_60_columns() {
        let v = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Working)
            .cloned()
            .unwrap();
        let wide = metadata(&v, false, 120);
        let mid = metadata(&v, false, 80);
        let narrow = metadata(&v, false, 59);
        assert!(wide.contains("claude-code"), "runtime survives when wide");
        assert!(narrow.is_empty(), "below 60 cols, drop metadata entirely");
        // Runtime drops before workspace; elapsed survives longest.
        assert!(mid.chars().count() <= wide.chars().count());
        assert!(!narrow.contains("ws:"));
    }

    #[test]
    fn the_title_never_touches_the_metadata() {
        let mut app = fixtures::app(fixtures::POPULATED);
        let buf = draw(&mut app, 80, 24);
        for (y, row) in app.rows.clone() {
            let Row::Task(id) = row else { continue };
            let v = app.views.iter().find(|v| v.id() == id).unwrap();
            let selected = app.selected_id.as_deref() == Some(id.as_str());
            let meta = metadata(v, selected, 80);
            if meta.is_empty() {
                continue;
            }
            let l = line(&buf, y);
            let meta_start = 80 - 1 - meta.chars().count() as u16;
            // The two cells before the metadata must be blank.
            for gap in 1..=TITLE_METADATA_GAP {
                let x = (meta_start - gap) as usize;
                assert_eq!(
                    l.chars().nth(x),
                    Some(' '),
                    "row {y} title runs into its metadata"
                );
            }
        }
    }

    #[test]
    fn a_configured_but_empty_board_names_the_config_file() {
        let mut app = fixtures::app(fixtures::EMPTY);
        let buf = draw(&mut app, 80, 24);
        assert!(line(&buf, 2).contains("Nothing queued"));
        assert!(line(&buf, 4).contains("routing.toml"));
        assert!(line(&buf, 5).contains("maps issues to a workspace"));
    }

    #[test]
    fn an_unconfigured_board_says_what_is_missing_and_what_to_run() {
        // Naming a config file the operator has not created yet is not an
        // instruction; this state has to be actionable.
        let mut app = fixtures::app(fixtures::EMPTY);
        app.setup_hints = vec![
            "No routing.toml yet — run  herdr-board init".into(),
            "No LINEAR_API_KEY, so Linear is never polled — add it to /cfg/.env".into(),
            "herdr-board doctor  checks all of this".into(),
        ];
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("not set up yet"), "{all}");
        assert!(all.contains("herdr-board init"), "{all}");
        assert!(all.contains("LINEAR_API_KEY"), "{all}");
        assert!(all.contains("herdr-board doctor"), "{all}");
        // And it does not fall back to the "configured but empty" copy.
        assert!(!all.contains("Issues assigned to you in Linear appear here"));
    }

    #[test]
    fn a_source_outage_keeps_the_rows_on_screen() {
        // Never blank the list because a poll failed.
        let mut app = fixtures::app(fixtures::LINEAR_DOWN);
        let buf = draw(&mut app, 80, 24);
        assert!(header_status(&app.sync).contains("linear ✗"));
        assert!(
            app.rows.iter().any(|(_, r)| matches!(r, Row::Task(_))),
            "rows must survive an outage"
        );
        let _ = buf;
    }

    #[test]
    fn a_ready_row_offers_a_route_to_detail() {
        // Otherwise detail is unreachable from the most common row type:
        // `enter` dispatches, and nothing else on screen mentions `l`.
        let mut app = fixtures::app(fixtures::POPULATED);
        app.selected_id = app
            .views
            .iter()
            .find(|v| v.state() == BoardState::Ready)
            .map(|v| v.id().to_string());
        let hints = footer_hints(&app);
        assert!(
            hints.iter().any(|(k, l, _)| *k == "l" && l.contains("detail")),
            "no detail hint on a ready row: {hints:?}"
        );
    }

    #[test]
    fn detail_offers_merge_when_the_task_has_a_pr() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Detail;
        app.selected_id = app
            .views
            .iter()
            .find(|v| v.task.pr_number.is_some())
            .map(|v| v.id().to_string());
        assert!(footer_hints(&app).iter().any(|(k, l, _)| *k == "m" && *l == "merge"));

        // ...and not otherwise.
        app.selected_id = app
            .views
            .iter()
            .find(|v| v.task.pr_number.is_none())
            .map(|v| v.id().to_string());
        assert!(!footer_hints(&app).iter().any(|(k, _, _)| *k == "m"));
    }

    #[test]
    fn every_section_header_toggles_both_ways() {
        let mut app = fixtures::app(fixtures::POPULATED);
        for state in BoardState::SECTION_ORDER {
            if app.section_len(state) == 0 {
                continue;
            }
            let id = crate::ui::state::section_row_id(state);
            app.selected_id = Some(id.clone());

            app.collapsed.insert(state);
            let folded = footer_hints(&app);
            assert!(folded.iter().any(|(_, l, _)| *l == "expand"), "{state}: {folded:?}");

            app.collapsed.remove(&state);
            let open = footer_hints(&app);
            assert!(open.iter().any(|(_, l, _)| *l == "collapse"), "{state}: {open:?}");

            // The header survives either way, or there is no way back.
            assert!(app.visible_task_ids().contains(&id), "{state} header vanished");
        }
    }

    #[test]
    fn a_folded_section_says_how_many_it_hides() {
        // Open, a count competes with the elapsed column for the eye. Folded,
        // it is the only thing left that says what is in there.
        let mut app = fixtures::app(fixtures::POPULATED);
        app.collapsed.insert(BoardState::Ready);
        let n = app.section_len(BoardState::Ready);
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains(&format!("{n} hidden")), "{all}");
    }

    #[test]
    fn a_review_row_only_promises_a_pr_when_there_is_one() {
        // Work reaches `review` on commits alone, and `o` then opens the issue.
        // A footer saying "open PR" in that case is simply a lie.
        let mut app = fixtures::app(fixtures::POPULATED);
        let with_pr = app
            .views
            .iter()
            .find(|v| v.state() == BoardState::Review && v.task.pr_number.is_some())
            .map(|v| v.id().to_string())
            .unwrap();
        app.selected_id = Some(with_pr);
        let hints = footer_hints(&app);
        assert!(hints.iter().any(|(_, l, _)| *l == "open PR"), "{hints:?}");
        assert!(hints.iter().any(|(k, _, _)| *k == "m"), "merge offered");

        // Strip the PR and the offer goes with it.
        for v in &mut app.views {
            v.task.pr_number = None;
            v.task.pr_url = None;
        }
        let hints = footer_hints(&app);
        assert!(hints.iter().any(|(_, l, _)| *l == "open issue"), "{hints:?}");
        assert!(!hints.iter().any(|(k, _, _)| *k == "m"), "nothing to merge");
    }

    #[test]
    fn a_review_row_says_when_a_pr_cannot_merge() {
        // Four branches merged to master tonight without seeing each other;
        // "open · waiting on you" is not the useful thing to say about one.
        let mut v = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Review && v.task.pr_number.is_some())
            .cloned()
            .unwrap();
        let n = v.task.pr_number.unwrap();

        v.repo = None;
        v.task.pr_mergeable = Some("behind".into());
        assert_eq!(metadata(&v, false, 80), format!("PR #{n} · behind master"));
        v.task.pr_mergeable = Some("dirty".into());
        assert_eq!(metadata(&v, false, 80), format!("PR #{n} · conflicts"));
        v.task.pr_mergeable = Some("clean".into());
        assert_eq!(metadata(&v, false, 80), format!("PR #{n} · waiting on you"));

        // ...and the repo leads when there is one to name.
        v.repo = Some("Tally".into());
        assert_eq!(metadata(&v, false, 80), format!("Tally PR #{n} · waiting on you"));
    }

    #[test]
    fn a_review_row_without_a_pr_names_its_branch() {
        let mut v = fixtures::app(fixtures::POPULATED)
            .views
            .iter()
            .find(|v| v.state() == BoardState::Review)
            .cloned()
            .unwrap();
        v.task.pr_number = None;
        v.branch = Some("board/age-6".into());
        assert_eq!(metadata(&v, false, 80), "board/age-6 · no PR");
    }

    #[test]
    fn footers_are_per_selection() {
        let mut app = fixtures::app(fixtures::POPULATED);
        for (state, expect) in [
            (BoardState::Ready, "dispatch"),
            (BoardState::Working, "cancel"),
            (BoardState::Failed, "retry"),
            (BoardState::Review, "open PR"),
        ] {
            let id = app
                .views
                .iter()
                .find(|v| v.state() == state)
                .map(|v| v.id().to_string());
            if let Some(id) = id {
                app.selected_id = Some(id);
                let hints = footer_hints(&app);
                assert!(
                    hints.iter().any(|(_, l, _)| l.contains(expect)),
                    "{state} footer is missing {expect}: {hints:?}"
                );
            }
        }
    }

    #[test]
    fn a_working_task_offers_no_enter_hint_in_the_prompt_view() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Prompt;
        app.selected_id = app
            .views
            .iter()
            .find(|v| v.state() == BoardState::Working)
            .map(|v| v.id().to_string());
        let hints = footer_hints(&app);
        assert!(
            !hints.iter().any(|(k, _, _)| *k == "enter"),
            "a working task must not offer enter: {hints:?}"
        );
    }

    #[test]
    fn every_footer_hint_is_a_click_target() {
        let mut app = fixtures::app(fixtures::POPULATED);
        draw(&mut app, 80, 24);
        assert_eq!(app.footer_hits.len(), footer_hints(&app).len());
        for (x1, x2, _) in &app.footer_hits {
            assert!(x2 > x1);
        }
    }

    #[test]
    fn the_prompt_view_shows_the_resolved_prompt_not_the_template() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Prompt;
        app.selected_id = app
            .views
            .iter()
            .find(|v| v.has_route)
            .map(|v| v.id().to_string());
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(!all.contains("{title}"), "template leaked into the view");
        assert!(all.contains("route"), "the matched route must be named");
    }

    #[test]
    fn the_prompt_view_states_plainly_when_there_is_no_route() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Prompt;
        app.selected_id = app
            .views
            .iter()
            .find(|v| !v.has_route)
            .map(|v| v.id().to_string());
        let buf = draw(&mut app, 80, 24);
        assert!(line(&buf, 3).contains("cannot be dispatched"));
    }

    #[test]
    fn the_stats_screen_reports_what_happened() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Stats;
        app.stats = Some(crate::stats::Stats {
            since_days: None,
            attempts: 11,
            tasks_touched: 10,
            outcomes: [("done".to_string(), 10), ("cancelled".to_string(), 1)]
                .into_iter()
                .collect(),
            live: 0,
            median_minutes: Some(17),
            longest_minutes: Some(43),
            completion_rate: Some(10.0 / 11.0),
            retried_tasks: 1,
            agent_dispatched: 2,
            by_workspace: [("herdr-board".to_string(), 6)].into_iter().collect(),
            by_runtime: [("claude-code".to_string(), 9)].into_iter().collect(),
        });
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("board ▸ stats"), "{all}");
        assert!(all.contains("11 dispatches across 10 tasks"), "{all}");
        assert!(all.contains("17m median"), "{all}");
        assert!(all.contains("91%"), "{all}");
        // The number that says whether the herd releases its own work.
        assert!(all.contains("2 by an agent"), "{all}");
        // No charts: the design forbids them.
        assert!(!all.contains('█') && !all.contains('▇'), "{all}");
    }

    #[test]
    fn the_stats_screen_says_so_when_nothing_has_run() {
        let mut app = fixtures::app(fixtures::EMPTY);
        app.screen = Screen::Stats;
        app.stats = None;
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("no dispatches yet"), "{all}");
    }

    #[test]
    fn help_says_the_prefix_passes_through() {
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Help;
        let buf = draw(&mut app, 80, 24);
        let all: String = (0..24).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n");
        assert!(all.contains("ctrl+b reaches herdr from here"));
    }

    #[test]
    fn renders_without_wrap_artifacts_at_every_supported_size() {
        for scenario in fixtures::ALL {
            let mut app = fixtures::app(scenario);
            for (w, h) in [(80u16, 24u16), (120, 40), (160, 38), (60, 20)] {
                for screen in [Screen::List, Screen::Detail, Screen::Prompt, Screen::Help] {
                    app.screen = screen;
                    let buf = draw(&mut app, w, h);
                    for y in 0..h {
                        let l: String = (0..w)
                            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                            .collect();
                        assert!(
                            l.chars().count() as u16 == w,
                            "{scenario} {screen:?} {w}x{h} row {y} is not exactly {w} cells"
                        );
                    }
                }
            }
        }
    }

    // ---- AGE-27: filtering ---------------------------------------------

    fn body(app: &mut App, w: u16, h: u16) -> String {
        let buf = draw(app, w, h);
        (0..h).map(|y| line(&buf, y)).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn the_header_says_which_filter_is_on_where_the_sync_status_sits() {
        // A filtered board that does not say so is a board that looks broken.
        let mut app = fixtures::app(fixtures::CROWDED);
        app.cycle_route();
        let head = line(&draw(&mut app, 100, 30), 0);
        assert!(head.contains("filter: altinn-forms"), "{head:?}");
        // ...and the status it shares the corner with is still there.
        assert!(head.contains("synced 12s"), "{head:?}");
        assert!(
            head.find("synced").unwrap() < head.find("filter:").unwrap(),
            "the filter has to hold the corner: {head:?}"
        );

        // A typed query says itself, `/` and all.
        app.clear_filter();
        app.open_find();
        for c in "altinn".chars() {
            app.filter_type(c);
        }
        app.accept_filter();
        assert!(line(&draw(&mut app, 100, 30), 0).contains("/altinn"));
    }

    #[test]
    fn the_filter_keeps_the_corner_when_the_pane_is_too_narrow_for_both() {
        // The status is a fact about the world that will still be true in a
        // minute; the filter is what the operator just did to this board.
        let mut app = fixtures::app(fixtures::CROWDED);
        app.cycle_route();
        let head = line(&draw(&mut app, 44, 24), 0);
        assert!(head.contains("filter: altinn-forms"), "{head:?}");
        assert!(!head.contains("synced"), "{head:?}");
        // ...and it never writes over the title.
        assert!(head.starts_with(" herdr-board"), "{head:?}");
    }

    #[test]
    fn the_field_is_a_line_on_the_footer_and_counts_as_you_type() {
        let mut app = fixtures::app(fixtures::CROWDED);
        app.open_find();
        for c in "signicat".chars() {
            app.filter_type(c);
        }
        let buf = draw(&mut app, 80, 24);
        let footer = line(&buf, 23);
        assert!(footer.starts_with(" /signicat"), "{footer:?}");
        assert!(footer.contains("esc clears"), "{footer:?}");
        assert!(
            footer.contains(&format!("{} rows", app.shown_tasks())),
            "the count that tells you when to stop typing: {footer:?}"
        );
        // The caret is emphasis, not a box or a background.
        let caret = &buf[(10, 23)];
        assert!(caret.modifier.contains(Modifier::REVERSED));
        assert_eq!(caret.bg, Color::Reset);
        // And the hints are gone: the keys are going into the field.
        assert!(!footer.contains("dispatch"), "{footer:?}");
    }

    #[test]
    fn the_key_reference_is_reachable_rather_than_cut_off() {
        // It binds more keys than a 24-row pane has lines. Stopping halfway
        // with nothing said is how a documented key goes missing — the same
        // failure as `m`, one screen further on.
        let mut app = fixtures::app(fixtures::POPULATED);
        app.screen = Screen::Help;

        let mut seen = String::new();
        for _ in 0..40 {
            seen.push_str(&body(&mut app, 80, 24));
            app.scroll_help(1);
        }
        for (key, desc) in help_lines() {
            if desc.is_empty() {
                continue;
            }
            assert!(seen.contains(desc), "`{key}` never comes into view: {desc:?}");
        }

        // ...and it says there is more, in the marker and in the footer, so
        // nobody has to guess that scrolling is a thing here.
        // The marker rides the rule, as it does on the list — `↓ ↑` also
        // appear as key names in the body, so that line is what to read.
        app.help_scroll = 0;
        let short = draw(&mut app, 80, 24);
        assert!(line(&short, 22).contains('↓'), "{:?}", line(&short, 22));
        assert!(line(&short, 23).contains("j k more keys"), "{:?}", line(&short, 23));

        // A pane tall enough for all of it says neither.
        let tall = draw(&mut app, 80, 40);
        assert!(!line(&tall, 38).contains('↓'), "{:?}", line(&tall, 38));
        assert!(!line(&tall, 39).contains("more keys"), "{:?}", line(&tall, 39));
    }

    #[test]
    fn an_open_field_with_nothing_typed_accuses_nobody() {
        // `/` on a board that was never set up must not overwrite the copy
        // saying so with a complaint about a query nobody has typed.
        let mut app = fixtures::app(fixtures::EMPTY);
        app.setup_hints = vec!["No routing.toml yet — run  herdr-board init".into()];
        app.open_find();
        let all = body(&mut app, 80, 24);
        assert!(all.contains("not set up yet"), "{all}");
        assert!(!all.contains("matches"), "{all}");
    }

    #[test]
    fn a_filter_that_hides_everything_says_which_keypress_did_that() {
        let mut app = fixtures::app(fixtures::CROWDED);
        app.unadopted.clear();
        app.open_find();
        for c in "zzzz".chars() {
            app.filter_type(c);
        }
        app.accept_filter();
        let all = body(&mut app, 80, 24);
        assert!(all.contains("No identifier, title or route matches"), "{all}");
        assert!(all.contains("F clears the filter"), "{all}");
    }

    #[test]
    fn filtering_to_one_route_leaves_only_that_routes_rows_and_sections() {
        // The board this exists for: 109 rows over five routes, down to one
        // project you can read at a glance.
        let mut app = fixtures::app(fixtures::CROWDED);
        app.filter = Filter::Route("tally".into());
        let all = body(&mut app, 100, 40);
        assert!(all.contains("WORKING"), "{all}");
        assert!(
            !all.contains("BLOCKED"),
            "a section with no rows left drew an empty header:\n{all}"
        );
        // Nothing from another route is on screen.
        for l in all.lines() {
            assert!(!l.contains("ws:itsm-agent"), "{l:?}");
        }
        // ...and the folded DONE header counts what is shown, not the board.
        assert!(
            !all.contains("DONE today"),
            "the done section is all itsm-agent rows:\n{all}"
        );
    }

    #[test]
    fn an_unadopted_repo_stays_visible_under_a_route_filter() {
        // It has no route by definition — that is the thing worth noticing.
        let mut app = fixtures::app(fixtures::CROWDED);
        app.filter = Filter::Route("tally".into());
        let all = body(&mut app, 100, 40);
        assert!(all.contains("UNADOPTED"), "{all}");
        assert!(all.contains("tripletex-mcp"), "{all}");
    }




    #[test]
    fn wrapping_respects_width_and_hard_breaks() {
        let lines = wrap("one two three four five", 9);
        assert!(lines.iter().all(|l| l.chars().count() <= 9), "{lines:?}");
        assert_eq!(wrap("a\nb", 40), vec!["a", "b"]);
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate("abc", 0), "");
    }
}
