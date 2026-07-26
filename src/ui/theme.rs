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

use crate::model::BoardState;
use ratatui::style::{Color, Modifier, Style};

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
    let c = match state {
        BoardState::Blocked | BoardState::Failed => Color::Red,
        BoardState::Working => Color::Yellow,
        BoardState::Review => Color::Cyan,
        // Nothing running yet, and closed issues are de-emphasized.
        BoardState::Ready => Color::Reset,
        BoardState::Done => return dim(),
    };
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
    fn working_is_yellow_and_review_is_cyan() {
        // Corrected from herdr's own src/ui/status.rs: the original brief had
        // working = green and blocked = yellow.
        assert_eq!(state_style(BoardState::Working).fg, Some(Color::Yellow));
        assert_eq!(state_style(BoardState::Review).fg, Some(Color::Cyan));
        assert_eq!(state_style(BoardState::Blocked).fg, Some(Color::Red));
    }
}
