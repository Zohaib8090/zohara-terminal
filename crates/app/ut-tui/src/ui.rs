//! The actual frame-drawing function, separate from `render.rs` (which
//! only converts one `Grid` into ratatui spans) and from `main.rs` (which
//! owns the real terminal / event loop). Kept as a plain function taking
//! `&mut Frame` so it can be exercised against `ratatui::backend::TestBackend`
//! in tests, with no real terminal involved.

use crate::render::{grid_to_text, pane_border_style};
use crate::{App, PromptState};
use ratatui::layout::Rect as RRect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use ut_ui::Rect as URect;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let effective = app.layout.effective_layout();
    let active_id = app.tab.active_pane().map(|p| p.id);

    let bounds = URect { x: 0.0, y: 0.0, w: area.width as f32, h: area.height.saturating_sub(if matches!(app.prompt, PromptState::Confirming { .. } | PromptState::ConfirmingUnknown) { 4 } else { 0 }) as f32 };

    for (pane_id, rect) in effective.rects(bounds) {
        let Some(terminal) = app.grid_for(pane_id) else { continue };
        let rrect = to_ratatui_rect(rect, area);
        let focused = Some(pane_id) == active_id;
        let block = Block::default().borders(Borders::ALL).border_style(pane_border_style(focused)).title(format!(" pane {pane_id} "));
        let inner = block.inner(rrect);
        frame.render_widget(block, rrect);
        let text = grid_to_text(terminal.grid());
        frame.render_widget(Paragraph::new(text), inner);
    }

    if let PromptState::Confirming { report_summary, .. } = &app.prompt {
        let prompt_area = RRect { x: 0, y: area.height.saturating_sub(4), width: area.width, height: 4 };
        let mut lines: Vec<Line> = vec![Line::styled("Destructive command — send anyway? (y/n)", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))];
        for s in report_summary {
            lines.push(Line::raw(s.clone()));
        }
        let block = Block::default().borders(Borders::ALL).title(" confirm ");
        frame.render_widget(Paragraph::new(Text::from(lines)).block(block), prompt_area);
    } else if app.prompt == PromptState::ConfirmingUnknown {
        let prompt_area = RRect { x: 0, y: area.height.saturating_sub(4), width: area.width, height: 4 };
        let lines: Vec<Line> = vec![
            Line::styled("Line edited in the shell (history/completion/search) — contents unknown.", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Line::raw("y = send Enter anyway, n = erase the line (Ctrl+U) and start over"),
        ];
        let block = Block::default().borders(Borders::ALL).title(" confirm ");
        frame.render_widget(Paragraph::new(Text::from(lines)).block(block), prompt_area);
    }
}

fn to_ratatui_rect(r: URect, bounds: RRect) -> RRect {
    RRect {
        x: bounds.x + r.x.round() as u16,
        y: bounds.y + r.y.round() as u16,
        width: r.w.round() as u16,
        height: r.h.round() as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn draws_a_single_pane_without_panicking() {
        let app = App::new(40, 10).unwrap();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        // The border title should appear somewhere in the rendered buffer.
        let content: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(content.contains("pane"));
    }

    #[test]
    fn draws_split_panes_side_by_side() {
        let mut app = App::new(60, 10).unwrap();
        app.split_active(ut_ui::Direction::Horizontal, 30, 10).unwrap();
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        // Two panes exist in the layout; just confirm no panic and both leaves render.
        assert_eq!(app.layout.layout.leaves().len(), 2);
    }

    #[test]
    fn draws_confirmation_banner_when_prompting() {
        let mut app = App::new(40, 10).unwrap();
        for c in "rm -rf /".chars() {
            app.handle_key(ut_input::Key::Char(c), ut_input::Modifiers::none());
        }
        app.handle_key(ut_input::Key::Enter, ut_input::Modifiers::none());
        assert!(matches!(app.prompt, PromptState::Confirming { .. }));

        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let content: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(content.contains("confirm"));
    }
}
