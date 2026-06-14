//! Error overlay renderer.
//!
//! The app stores the most recent error in [`crate::state::app_state::AppState`]
//! as `error_message`. While it is set, `handle_key` swallows every key
//! except Esc/Enter so the message can't be dismissed by accident. That
//! contract only makes sense if the message is actually *visible* — a
//! 40-char truncation in the status-bar corner left users staring at a
//! UI that ignored every keystroke with no hint why. This overlay shows
//! the full message, word-wrapped, with an explicit dismiss hint.

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Widget},
};

const ERROR_FG: Color = Color::Rgb(255, 120, 120);
const BG: Color = Color::Rgb(36, 18, 18);
const FG_PRIMARY: Color = Color::Rgb(235, 220, 220);
const ACCENT: Color = Color::Rgb(255, 180, 120);

/// Render the error message centred inside `area`.
pub fn render_error(area: Rect, buf: &mut Buffer, message: &str) {
    // Choose a comfortable reading width, clamped to the terminal.
    let outer_w = 64u16.min(area.width.max(20));
    // Wrap the message to the inner text width (minus borders + padding).
    let text_w = outer_w.saturating_sub(4).max(1) as usize;
    let lines = wrap_words(message, text_w);

    // Height: top + bottom border (2) + a blank spacer + body lines +
    // a blank spacer + footer hint. Clamp the body so a pathological
    // message can't grow taller than the screen.
    let max_body = area.height.saturating_sub(5).max(1) as usize;
    let body_len = lines.len().min(max_body);
    let outer_h = (body_len as u16 + 5).min(area.height.max(3));

    let popup = centered(area, outer_w, outer_h);
    Clear.render(popup, buf);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ERROR_FG).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(BG))
        .title(Span::styled(
            " ⚠ Error ",
            Style::default().fg(ERROR_FG).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    block.render(popup, buf);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    // Body, starting one row down for breathing room.
    let mut y = inner.y + 1;
    let footer_y = inner.y + inner.height - 1;
    for line in lines.iter().take(body_len) {
        if y >= footer_y {
            break;
        }
        buf.set_line(
            inner.x + 1,
            y,
            &Line::from(Span::styled(
                line.clone(),
                Style::default().fg(FG_PRIMARY).bg(BG),
            )),
            inner.width.saturating_sub(1),
        );
        y += 1;
    }

    // Footer hint — always on the bottom inner row.
    let hint = Line::from(vec![
        Span::styled(
            " [Esc / Enter] ",
            Style::default()
                .fg(ACCENT)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("dismiss ", Style::default().fg(FG_PRIMARY).bg(BG)),
    ]);
    buf.set_line(inner.x + 1, footer_y, &hint, inner.width.saturating_sub(1));
}

/// Greedy word-wrap to `width` columns. Words longer than `width` are
/// hard-split so they can't overflow the popup.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        // Hard-split a word that is itself wider than the line.
        if word.chars().count() > width {
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == width {
                    lines.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            current = chunk;
            continue;
        }

        let extra = if current.is_empty() {
            word.chars().count()
        } else {
            word.chars().count() + 1
        };
        if current.chars().count() + extra > width {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Centre a `w × h` rectangle inside `area`, clamped to the terminal.
fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_splits_on_word_boundaries() {
        let lines = wrap_words("the quick brown fox jumps", 9);
        assert!(lines.iter().all(|l| l.chars().count() <= 9));
        // Reassembling with spaces restores the original text.
        assert_eq!(lines.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn wrap_hard_splits_overlong_words() {
        let lines = wrap_words("supercalifragilistic", 5);
        assert!(lines.iter().all(|l| l.chars().count() <= 5));
        assert_eq!(lines.concat(), "supercalifragilistic");
    }

    #[test]
    fn wrap_handles_real_paused_message() {
        let msg = "Database 'space-dungeon' is paused. SpacetimeDB Maincloud \
                   suspends inactive databases; resume it from the dashboard \
                   at https://spacetimedb.com (or republish it), then reconnect.";
        let lines = wrap_words(msg, 60);
        assert!(lines.len() > 1, "long message should wrap to many lines");
        assert!(lines.iter().all(|l| l.chars().count() <= 60));
    }

    #[test]
    fn wrap_zero_width_is_safe() {
        let lines = wrap_words("anything", 0);
        assert_eq!(lines, vec!["anything".to_string()]);
    }

    #[test]
    fn wrap_empty_yields_one_blank_line() {
        assert_eq!(wrap_words("", 10), vec![String::new()]);
    }

    /// Flatten a rendered buffer into a single string so we can assert
    /// on the visible text regardless of where it wrapped.
    fn buffer_text(buf: &Buffer) -> String {
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn render_shows_full_message_and_dismiss_hint() {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let msg = "Database 'space-dungeon' is paused. SpacetimeDB Maincloud \
                   suspends inactive databases; resume it from the dashboard \
                   at https://spacetimedb.com (or republish it), then reconnect.";
        render_error(area, &mut buf, msg);

        let text = buffer_text(&buf);
        // The full message is visible (not truncated to 40 chars), even
        // though it wraps across several lines.
        assert!(text.contains("space-dungeon"));
        assert!(text.contains("paused"));
        assert!(text.contains("reconnect"));
        // And the user is told how to get out.
        assert!(text.contains("dismiss"));
        assert!(text.contains("Esc"));
    }

    #[test]
    fn render_survives_tiny_terminal() {
        // A 10x3 terminal must not panic or overflow the buffer.
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        render_error(area, &mut buf, "some long error message that cannot fit");
        // No assertion beyond "did not panic".
    }
}
