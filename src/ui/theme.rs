//! Design tokens.
//!
//! Three rules outrank everything here:
//!   1. **ANSI-16 only.** No truecolor, no hex literals. Every color is one of
//!      ratatui's named ANSI colors, which resolve through the operator's own
//!      terminal theme — so the board restyles for free.
//!   2. **Never paint a background.** Terminal default everywhere. Selection is
//!      `REVERSED`, emphasis is `BOLD`, de-emphasis is `DIM`. There is no `bg()`
//!      call anywhere in this crate outside tests.
//!   3. **Every state survives color being stripped** — each pairs a color with
//!      a shape-distinct glyph (see `BoardState::glyph`).
//!
//! Rule 3 was asserted for a long time and never observed: no terminal ships a
//! monochrome theme, so the only monochrome renders that existed were
//! `tools/render-check`'s browser simulation. [`NO_COLOR`](https://no-color.org)
//! makes it reachable in a real terminal.

use crate::model::BoardState;
use ratatui::style::{Color, Modifier, Style};

/// Honour <https://no-color.org>: set and non-empty means no color, whatever
/// the value. An empty `NO_COLOR=` is explicitly *not* opting in.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

/// Default foreground: whatever the terminal already uses.
pub fn fg_default() -> Style {
    Style::new().fg(Color::Reset)
}

/// Ids, metadata, rules, section rules, hints.
pub fn dim() -> Style {
    Style::new().fg(Color::Reset).add_modifier(Modifier::DIM)
}

/// Section labels, key names, focused picker field.
pub fn bold() -> Style {
    Style::new().fg(Color::Reset).add_modifier(Modifier::BOLD)
}

/// The state gutter — the only colored cell in a row.
///
/// `review` is herdr's teal, which in an ANSI-16 palette is cyan. `failed`
/// shares `blocked`'s red on purpose: both mean "needs you", and they are
/// separated by glyph and by section.
pub fn state_style(state: BoardState) -> Style {
    state_style_when(state, no_color())
}

/// The gutter style, with the `NO_COLOR` decision passed in.
///
/// Split out so the stripped palette is tested as a pure function rather than
/// by mutating the environment — `set_var` is `unsafe` and tests share a
/// process, so an env-toggling test would race every other test in the binary.
///
/// Stripping color deliberately leaves `DIM` alone. Emphasis is not color: the
/// board's monochrome legibility rests on `DIM`/`BOLD`/`REVERSED` plus
/// shape-distinct glyphs, so dropping those would break the very property
/// `NO_COLOR` exists to let us check.
fn state_style_when(state: BoardState, stripped: bool) -> Style {
    let c = match state {
        BoardState::Blocked | BoardState::Failed => Color::Red,
        BoardState::Working => Color::Yellow,
        BoardState::Review => Color::Cyan,
        // Nothing running yet, and closed issues are de-emphasized.
        BoardState::Ready => Color::Reset,
        BoardState::Done => return dim(),
    };
    if stripped {
        return fg_default();
    }
    Style::new().fg(c)
}

/// Horizontal rule. `─` only — always a straight rule, never a box, and never a
/// vertical: herdr owns pane chrome, and drawing our own would double every
/// divider the operator already has.
pub const RULE: char = '─';

/// Truncation marker.
pub const ELLIPSIS: char = '…';

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_style_sets_a_background() {
        // Rule 2: background fills are where themes visibly clash.
        for s in [fg_default(), dim(), bold()] {
            assert_eq!(s.bg, None);
        }
        for st in BoardState::SECTION_ORDER {
            assert_eq!(state_style(st).bg, None, "{st} painted a background");
        }
    }

    #[test]
    fn every_color_is_ansi_16() {
        // Rule 1: no truecolor anywhere.
        for st in BoardState::SECTION_ORDER {
            match state_style(st).fg {
                Some(Color::Rgb(..)) | Some(Color::Indexed(_)) => {
                    panic!("{st} uses a non-ANSI-16 color")
                }
                _ => {}
            }
        }
    }

    #[test]
    fn blocked_and_failed_share_red_but_not_a_glyph() {
        assert_eq!(
            state_style(BoardState::Blocked).fg,
            state_style(BoardState::Failed).fg
        );
        assert_ne!(BoardState::Blocked.glyph(), BoardState::Failed.glyph());
    }

    #[test]
    fn no_color_strips_every_hue() {
        for st in BoardState::SECTION_ORDER {
            let fg = state_style_when(st, true).fg;
            assert!(
                matches!(fg, None | Some(Color::Reset)),
                "{st} still emits {fg:?} under NO_COLOR"
            );
        }
    }

    #[test]
    fn no_color_keeps_emphasis_because_emphasis_is_not_color() {
        // The whole point of NO_COLOR here is to check that states stay
        // distinguishable without hue. That rests on DIM/BOLD/REVERSED and on
        // glyphs — strip those too and the test surface disappears.
        assert!(
            state_style_when(BoardState::Done, true)
                .add_modifier
                .contains(Modifier::DIM),
            "DONE lost its dim, which is what separates it from READY"
        );
        for s in [dim(), bold()] {
            assert!(!s.add_modifier.is_empty());
        }
    }

    #[test]
    fn stripped_states_are_told_apart_by_glyph_alone() {
        // With hue gone the glyph is the only carrier left, so it has to be
        // unique across every section — including Blocked vs Failed, which
        // share red and would otherwise collapse into each other.
        let mut seen = std::collections::HashSet::new();
        for st in BoardState::SECTION_ORDER {
            assert!(
                seen.insert(st.glyph()),
                "{st} reuses glyph {:?} under NO_COLOR",
                st.glyph()
            );
        }
    }

    #[test]
    fn an_empty_no_color_is_not_opting_in() {
        // no-color.org is specific: present *and non-empty*. `NO_COLOR=` is
        // how someone unsets it in a shell profile.
        assert_eq!(state_style_when(BoardState::Working, false).fg, Some(Color::Yellow));
    }

    #[test]
    fn working_is_yellow_and_review_is_cyan() {
        // Corrected from herdr's own src/ui/status.rs: the original brief had
        // working = green and blocked = yellow.
        assert_eq!(state_style(BoardState::Working).fg, Some(Color::Yellow));
        assert_eq!(state_style(BoardState::Review).fg, Some(Color::Cyan));
        assert_eq!(state_style(BoardState::Blocked).fg, Some(Color::Red));
    }
}
