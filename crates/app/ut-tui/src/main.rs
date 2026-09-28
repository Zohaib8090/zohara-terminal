//! Thin binary: owns the real terminal (raw mode, alternate screen) and
//! the crossterm event loop; all actual state and logic lives in `ut_tui`
//! (the library half of this crate), which is what's unit-tested.
//!
//! Keybindings:
//!   Ctrl+B then "  split horizontally, Ctrl+B then -  split vertically
//!   Ctrl+B then x  close the active pane
//!   Ctrl+B then z  toggle zoom on the active pane
//!   Ctrl+B then q  quit
//! (tmux-style prefix key, so ordinary Ctrl+B-less input always reaches the shell)

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal as RatatuiTerminal;
use std::io::{self, stdout};
use std::time::Duration;
use ut_input::{Key as UtKey, Modifiers as UtMods};
use ut_tui::App;
use ut_ui::Direction;

fn main() -> io::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut app = App::new(cols, rows)?;
    for pane in app.tab.panes_mut() {
        pane.set_nonblocking(true)?;
    }

    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = RatatuiTerminal::new(backend)?;

    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

fn run(terminal: &mut RatatuiTerminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    let mut prefix_pending = false;

    loop {
        app.pump_all();
        terminal.draw(|f| ut_tui::draw(f, app))?;

        if app.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(30))? {
            if let Event::Key(key_event) = event::read()? {
                if key_event.kind != KeyEventKind::Press {
                    continue;
                }

                if prefix_pending {
                    prefix_pending = false;
                    match key_event.code {
                        KeyCode::Char('"') => app.split_active(Direction::Vertical, 40, 12)?,
                        KeyCode::Char('-') => app.split_active(Direction::Horizontal, 40, 12)?,
                        KeyCode::Char('x') => app.close_active(),
                        KeyCode::Char('z') => app.zoom_active(),
                        KeyCode::Char('q') => {
                            app.should_quit = true;
                        }
                        _ => {}
                    }
                    continue;
                }

                if key_event.code == KeyCode::Char('b') && key_event.modifiers.contains(KeyModifiers::CONTROL) {
                    prefix_pending = true;
                    continue;
                }

                if let Some((key, mods)) = translate(key_event.code, key_event.modifiers) {
                    app.handle_key(key, mods);
                }
            }
        }
    }
}

fn translate(code: KeyCode, mods: KeyModifiers) -> Option<(UtKey, UtMods)> {
    let ut_mods = UtMods { ctrl: mods.contains(KeyModifiers::CONTROL), alt: mods.contains(KeyModifiers::ALT), shift: mods.contains(KeyModifiers::SHIFT) };
    let key = match code {
        KeyCode::Char(c) => UtKey::Char(c),
        KeyCode::Enter => UtKey::Enter,
        KeyCode::Backspace => UtKey::Backspace,
        KeyCode::Delete => UtKey::Delete,
        KeyCode::Tab => UtKey::Tab,
        KeyCode::Esc => UtKey::Escape,
        KeyCode::Up => UtKey::Up,
        KeyCode::Down => UtKey::Down,
        KeyCode::Left => UtKey::Left,
        KeyCode::Right => UtKey::Right,
        KeyCode::Home => UtKey::Home,
        KeyCode::End => UtKey::End,
        KeyCode::PageUp => UtKey::PageUp,
        KeyCode::PageDown => UtKey::PageDown,
        KeyCode::Insert => UtKey::Insert,
        KeyCode::F(n) => UtKey::Function(n),
        _ => return None,
    };
    Some((key, ut_mods))
}
