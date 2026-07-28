//! The dispatch picker: the **only** modal in the plugin.
//!
//! It runs as a herdr `placement = "popup"` pane, so **herdr draws the border** —
//! this module renders only the interior. A popup is not a herdr pane: it has no
//! pane id and is outside every pane/agent API, so it cannot reconcile anything
//! itself. It performs the dispatch and exits; the daemon and the board pick the
//! result up from the database.

use super::render::{put, put_right, rule, truncate};
use super::theme;
use crate::dispatch::{Overrides, Plan};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Workspace,
    Runtime,
    Branch,
}

impl Field {
    pub const ORDER: [Field; 3] = [Field::Workspace, Field::Runtime, Field::Branch];

    pub fn label(self) -> &'static str {
        match self {
            Field::Workspace => "workspace",
            Field::Runtime => "runtime",
            Field::Branch => "branch",
        }
    }

    /// Only workspace and runtime cycle; the branch is free text.
    pub fn cyclable(self) -> bool {
        !matches!(self, Field::Branch)
    }
}

pub struct PickerState {
    pub identifier: String,
    pub title: String,
    pub workspaces: Vec<String>,
    pub runtimes: Vec<String>,
    pub workspace_ix: usize,
    pub runtime_ix: usize,
    pub branch: String,
    pub focus: usize,
    /// Recomputed whenever the workspace changes.
    pub plan: Plan,
    /// A refusal to show in place of the consequences line.
    pub message: Option<String>,
}

impl PickerState {
    pub fn workspace(&self) -> &str {
        &self.workspaces[self.workspace_ix.min(self.workspaces.len() - 1)]
    }

    pub fn runtime(&self) -> &str {
        &self.runtimes[self.runtime_ix.min(self.runtimes.len() - 1)]
    }

    pub fn focused(&self) -> Field {
        Field::ORDER[self.focus.min(Field::ORDER.len() - 1)]
    }

    pub fn overrides(&self) -> Overrides {
        Overrides {
            workspace: Some(self.workspace().to_string()),
            runtime: Some(self.runtime().to_string()),
            branch: Some(self.branch.clone()),
            // The picker is the operator's own path; a popup owns no attempt,
            // so provenance resolves to "you" without saying so.
            via: None,
        }
    }

    pub fn next_field(&mut self) {
        self.focus = (self.focus + 1) % Field::ORDER.len();
    }

    pub fn prev_field(&mut self) {
        self.focus = (self.focus + Field::ORDER.len() - 1) % Field::ORDER.len();
    }

    /// `←`/`→` cycle the focused field. Returns true when the choice changed, so
    /// the caller can re-plan (the cap and the worktree path depend on it).
    pub fn cycle(&mut self, delta: isize) -> bool {
        match self.focused() {
            Field::Workspace => {
                if self.workspaces.len() < 2 {
                    return false;
                }
                let n = self.workspaces.len() as isize;
                self.workspace_ix = ((self.workspace_ix as isize + delta).rem_euclid(n)) as usize;
                true
            }
            Field::Runtime => {
                if self.runtimes.len() < 2 {
                    return false;
                }
                let n = self.runtimes.len() as isize;
                self.runtime_ix = ((self.runtime_ix as isize + delta).rem_euclid(n)) as usize;
                true
            }
            Field::Branch => false,
        }
    }

    pub fn type_char(&mut self, c: char) {
        if self.focused() == Field::Branch && !c.is_control() {
            self.branch.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.focused() == Field::Branch {
            self.branch.pop();
        }
    }

    /// Which field a click at interior row `y` focuses.
    pub fn field_at_row(y: u16) -> Option<usize> {
        match y {
            4 => Some(0),
            5 => Some(1),
            6 => Some(2),
            _ => None,
        }
    }
}

/// The consequences line: whether the target workspace has room.
pub fn capacity_line(plan: &Plan) -> String {
    if plan.at_cap() {
        // At cap the picker states the fact rather than greying out a key that
        // still appears to be offered.
        format!(
            "ws:{} is at {} of {} working — cancel one first",
            plan.workspace, plan.live_in_workspace, plan.max_concurrent
        )
    } else {
        format!(
            "ws:{}  {} of {} working",
            plan.workspace, plan.live_in_workspace, plan.max_concurrent
        )
    }
}

/// Footer keys. `enter` is **removed** at cap, not greyed out.
pub fn footer(plan: &Plan) -> Vec<(&'static str, &'static str)> {
    if plan.at_cap() {
        vec![
            ("←→", "change workspace"),
            ("tab", "next field"),
            ("esc", "cancel"),
        ]
    } else {
        vec![
            ("enter", "dispatch"),
            ("←→", "change"),
            ("tab", "next field"),
            ("esc", "cancel"),
        ]
    }
}

pub fn render(buf: &mut Buffer, area: Rect, st: &PickerState) {
    let right = area.width.saturating_sub(2);

    // row 1: title
    let mut x = 2;
    x += put(buf, area, x, 1, "dispatch", theme::fg_default());
    // The route is the part of a dispatch that can be wrong, and it is
    // otherwise invisible — first matching route wins, and nothing else in the
    // picker says which one did.
    let route = format!("route  {}", st.plan.route_name);
    let route_w = route.chars().count() as u16;
    put(
        buf,
        area,
        x + 1,
        1,
        &truncate(
            &format!("{}  {}", st.identifier, st.title),
            right.saturating_sub(x + 1 + route_w + 2) as usize,
        ),
        theme::dim(),
    );
    put_right(buf, area, right, 1, &route, theme::dim());

    rule(buf, area, 2, right, 2, theme::dim());

    // rows 4-6: the fields. Labels dim at col 2 (12 cells), values at col 14.
    for (i, field) in Field::ORDER.iter().enumerate() {
        let y = 4 + i as u16;
        let focused = st.focus == i;
        put(buf, area, 2, y, field.label(), theme::dim());
        let value = match field {
            Field::Workspace => format!("ws:{}", st.workspace()),
            Field::Runtime => st.runtime().to_string(),
            Field::Branch => st.branch.clone(),
        };
        put(buf, area, 14, y, &value, theme::fg_default());
        if field.cyclable() {
            put_right(buf, area, right, y, "‹ ›", theme::dim());
        }
        if focused {
            // The focused field reverses across the interior width.
            for x in 1..area.width.saturating_sub(1) {
                if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                    cell.modifier.remove(Modifier::DIM);
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }

    rule(buf, area, 2, right, 8, theme::dim());

    // rows 9-10: the two consequences the operator cannot see anywhere else —
    // the worktree that gets created, and whether the workspace has room.
    let wt = st
        .plan
        .worktree
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    put(buf, area, 6, 9, &format!("worktree  wt/{wt}"), theme::dim());

    let cap = capacity_line(&st.plan);
    let cap_style = if st.plan.at_cap() {
        theme::fg_default()
    } else {
        theme::dim()
    };
    put(buf, area, 6, 10, &cap, cap_style);

    // A refusal (duplicate dispatch, or a failed dispatch) surfaces here rather
    // than in a second dialog.
    if let Some(msg) = &st.message {
        put(buf, area, 2, 11, &truncate(msg, right as usize), theme::fg_default());
    }

    let mut x = 2;
    for (i, (key, label)) in footer(&st.plan).into_iter().enumerate() {
        if i > 0 {
            x += put(buf, area, x, 12, " · ", theme::dim());
        }
        x += put(buf, area, x, 12, key, theme::fg_default());
        x += put(buf, area, x, 12, &format!(" {label}"), theme::dim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn plan(live: usize, max: usize) -> Plan {
        Plan {
            identifier: "LIN-145".into(),
            route_name: "fintech".into(),
            workspace: "fintech".into(),
            repo: PathBuf::from("/repo"),
            runtime: "claude-code".into(),
            herdr_kind: "claude",
            branch: "board/lin-145".into(),
            worktree: PathBuf::from("/state/wt/lin-145-1"),
            prompt: "do it".into(),
            attempt_no: 1,
            live_in_workspace: live,
            max_concurrent: max,
            dispatcher: crate::model::Dispatcher::Operator,
        }
    }

    fn state(live: usize, max: usize) -> PickerState {
        PickerState {
            identifier: "LIN-145".into(),
            title: "Add retry to Altinn poller".into(),
            workspaces: vec!["fintech".into(), "offhand".into()],
            runtimes: vec!["claude-code".into(), "codex".into()],
            workspace_ix: 0,
            runtime_ix: 0,
            branch: "board/lin-145".into(),
            focus: 0,
            plan: plan(live, max),
            message: None,
        }
    }

    fn draw(st: &PickerState) -> Buffer {
        let area = Rect::new(0, 0, 60, 14);
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, st);
        buf
    }

    fn line(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn the_picker_draws_no_border_of_its_own() {
        // herdr draws the popup border; drawing our own nests boxes.
        let buf = draw(&state(1, 2));
        for y in 0..14 {
            let l = line(&buf, y);
            for ch in ['│', '┌', '┐', '└', '┘', '├', '┤'] {
                assert!(!l.contains(ch), "row {y} drew border {ch:?}: {l:?}");
            }
        }
    }

    #[test]
    fn fields_sit_on_their_specified_rows_and_columns() {
        let buf = draw(&state(1, 2));
        assert!(line(&buf, 4).starts_with("  workspace"));
        assert!(line(&buf, 5).starts_with("  runtime"));
        assert!(line(&buf, 6).starts_with("  branch"));
        // Values at col 14.
        assert_eq!(&line(&buf, 4)[14..17], "ws:");
    }

    #[test]
    fn only_cyclable_fields_are_marked() {
        let buf = draw(&state(1, 2));
        assert!(line(&buf, 4).ends_with("‹ ›"));
        assert!(line(&buf, 5).ends_with("‹ ›"));
        assert!(!line(&buf, 6).ends_with("‹ ›"), "branch is not cyclable");
    }

    #[test]
    fn the_focused_field_is_reversed_across_the_interior() {
        let mut st = state(1, 2);
        st.focus = 1;
        let buf = draw(&st);
        for x in 1..59 {
            assert!(
                buf[(x, 5)].modifier.contains(Modifier::REVERSED),
                "cell {x} of the focused field is not reversed"
            );
        }
        assert!(!buf[(2, 4)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_picker_names_the_matched_route() {
        let buf = draw(&state(1, 2));
        assert!(line(&buf, 1).ends_with("route  fintech"), "{:?}", line(&buf, 1));
        // ...without crowding out the title.
        assert!(line(&buf, 1).contains("LIN-145"));
    }

    #[test]
    fn the_picker_shows_both_consequences() {
        let buf = draw(&state(1, 2));
        assert!(line(&buf, 9).contains("worktree  wt/lin-145-1"));
        assert!(line(&buf, 10).contains("ws:fintech  1 of 2 working"));
    }

    #[test]
    fn at_cap_states_the_fact_and_removes_enter() {
        let st = state(2, 2);
        assert!(st.plan.at_cap());
        assert_eq!(
            capacity_line(&st.plan),
            "ws:fintech is at 2 of 2 working — cancel one first"
        );
        let keys: Vec<&str> = footer(&st.plan).iter().map(|(k, _)| *k).collect();
        assert!(
            !keys.contains(&"enter"),
            "at cap, enter must be removed, not greyed out: {keys:?}"
        );
        assert!(keys.contains(&"esc"));
    }

    #[test]
    fn below_cap_enter_is_offered() {
        let keys: Vec<&str> = footer(&state(1, 2).plan).iter().map(|(k, _)| *k).collect();
        assert_eq!(keys[0], "enter");
    }

    #[test]
    fn cycling_wraps_in_both_directions() {
        let mut st = state(1, 2);
        assert_eq!(st.workspace(), "fintech");
        assert!(st.cycle(1));
        assert_eq!(st.workspace(), "offhand");
        assert!(st.cycle(1));
        assert_eq!(st.workspace(), "fintech", "cycling wraps");
        assert!(st.cycle(-1));
        assert_eq!(st.workspace(), "offhand");
    }

    #[test]
    fn the_branch_field_takes_text_not_cycles() {
        let mut st = state(1, 2);
        st.focus = 2;
        assert!(!st.cycle(1), "branch must not cycle");
        st.backspace();
        st.type_char('x');
        assert_eq!(st.branch, "board/lin-14x");
    }

    #[test]
    fn tab_moves_between_fields_and_wraps() {
        let mut st = state(1, 2);
        st.next_field();
        assert_eq!(st.focused(), Field::Runtime);
        st.next_field();
        st.next_field();
        assert_eq!(st.focused(), Field::Workspace);
        st.prev_field();
        assert_eq!(st.focused(), Field::Branch);
    }

    #[test]
    fn clicking_a_row_focuses_that_field() {
        assert_eq!(PickerState::field_at_row(4), Some(0));
        assert_eq!(PickerState::field_at_row(6), Some(2));
        assert_eq!(PickerState::field_at_row(9), None);
    }

    #[test]
    fn a_refusal_surfaces_in_the_picker_not_a_dialog() {
        let mut st = state(1, 2);
        st.message = Some("LIN-145 already has a live attempt".into());
        let buf = draw(&st);
        assert!(line(&buf, 11).contains("already has a live attempt"));
    }

    #[test]
    fn overrides_carry_all_three_choices() {
        let mut st = state(1, 2);
        st.cycle(1);
        let ov = st.overrides();
        assert_eq!(ov.workspace.as_deref(), Some("offhand"));
        assert_eq!(ov.runtime.as_deref(), Some("claude-code"));
        assert_eq!(ov.branch.as_deref(), Some("board/lin-145"));
    }
}
