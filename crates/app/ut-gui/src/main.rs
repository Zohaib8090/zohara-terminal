//! Thin binary: owns the native window via `eframe`; all pane/session/
//! safety logic is `ut_tui::App` (shared with the TUI front end — see
//! `lib.rs`). Only rendering and input translation are new here.
//!
//! Also wires in `ut-palette` as a real Ctrl+P command palette over the
//! same split/close/zoom/quit actions the Ctrl+B prefix already exposes —
//! two input paths to the same `ut_tui::App` calls, not two
//! implementations of "what a split does".
//!
//! Verification flags (used in development, safe to leave in for anyone
//! else who wants to re-verify without a physical display):
//!   --screenshot <path>          save a PNG once the window has settled
//!   --exit-after-frames <n>      take the screenshot on frame n, then exit
//!   --auto-type "<text>"         type this into the active pane (then Enter)
//!                                 a few frames in, to prove keyboard input
//!                                 and PTY output both actually work
//!   --open-palette "<query>"     open the command palette and type this
//!                                 query a few frames in, for screenshotting
//!                                 the palette itself without simulating a
//!                                 real Ctrl+P keypress through Xvfb
//!
//! Example, entirely headless:
//!   xvfb-run -a cargo run -p ut-gui -- --screenshot /tmp/out.png \
//!       --exit-after-frames 40 --auto-type 'echo gui-integration-ok'

use egui::{Color32, FontId, Key as EKey};
use std::collections::HashMap;
use std::path::PathBuf;
use ut_input::{Key as UtKey, Modifiers as UtMods};
use ut_palette::{Category, Palette, PaletteItem, PaletteState};
use ut_tui::{App, PromptState};
use ut_ui::{Direction as SplitDirection, Rect as URect};

struct Args {
    screenshot: Option<PathBuf>,
    exit_after_frames: Option<u32>,
    auto_type: Option<String>,
    auto_split: Option<SplitDirection>,
    open_palette: Option<String>,
    palette_confirm: bool,
}

fn parse_args() -> Args {
    let mut screenshot = None;
    let mut exit_after_frames = None;
    let mut auto_type = None;
    let mut auto_split = None;
    let mut open_palette = None;
    let mut palette_confirm = false;
    let raw: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--screenshot" => {
                screenshot = raw.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            "--exit-after-frames" => {
                exit_after_frames = raw.get(i + 1).and_then(|s| s.parse().ok());
                i += 2;
            }
            "--auto-type" => {
                auto_type = raw.get(i + 1).cloned();
                i += 2;
            }
            "--auto-split" => {
                auto_split = match raw.get(i + 1).map(|s| s.as_str()) {
                    Some("horizontal") => Some(SplitDirection::Horizontal),
                    Some("vertical") => Some(SplitDirection::Vertical),
                    _ => None,
                };
                i += 2;
            }
            "--open-palette" => {
                open_palette = raw.get(i + 1).cloned();
                i += 2;
            }
            "--palette-confirm" => {
                palette_confirm = true;
                i += 1;
            }
            _ => i += 1,
        }
    }
    Args { screenshot, exit_after_frames, auto_type, auto_split, open_palette, palette_confirm }
}

fn build_palette() -> Palette {
    let mut palette = Palette::new();
    palette.set_items(vec![
        PaletteItem::new("split-h", "Split Pane Horizontally", Category::Action).with_keyword("vsplit"),
        PaletteItem::new("split-v", "Split Pane Vertically", Category::Action).with_keyword("hsplit"),
        PaletteItem::new("close-pane", "Close Active Pane", Category::Action),
        PaletteItem::new("toggle-zoom", "Toggle Zoom on Active Pane", Category::Action),
        PaletteItem::new("quit", "Quit Zohara Terminal", Category::Action),
    ]);
    palette.add_alias("h", "split-h");
    palette.add_alias("v", "split-v");
    palette
}

fn main() -> eframe::Result<()> {
    let args = parse_args();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 640.0]).with_title("Zohara Terminal"),
        ..Default::default()
    };
    eframe::run_native(
        "Zohara Terminal",
        options,
        Box::new(move |_cc| {
            let app = App::new(100, 30).expect("spawn initial pane");
            for pane in app.tab.panes().iter() {
                let _ = pane; // sizes set at spawn; per-pane nonblocking set below via tab
            }
            Ok(Box::new(GuiApp::new(app, args)))
        }),
    )
}

struct GuiApp {
    app: App,
    prefix_pending: bool,
    font_size: f32,
    char_size: Option<egui::Vec2>,
    last_pane_sizes: HashMap<u64, (u16, u16)>,
    screenshot_path: Option<PathBuf>,
    exit_after_frames: Option<u32>,
    auto_type: Option<String>,
    auto_typed: bool,
    auto_split: Option<SplitDirection>,
    auto_split_done: bool,
    frame_count: u32,
    screenshot_saved: bool,
    palette: Palette,
    palette_state: PaletteState,
    palette_open: bool,
    open_palette_arg: Option<String>,
    open_palette_done: bool,
    palette_confirm: bool,
    palette_confirm_done: bool,
}

impl GuiApp {
    fn new(mut app: App, args: Args) -> Self {
        for pane in app.tab.panes_mut() {
            let _ = pane.set_nonblocking(true);
        }
        GuiApp {
            app,
            prefix_pending: false,
            font_size: 15.0,
            char_size: None,
            last_pane_sizes: HashMap::new(),
            screenshot_path: args.screenshot,
            exit_after_frames: args.exit_after_frames,
            auto_type: args.auto_type,
            auto_typed: false,
            auto_split: args.auto_split,
            auto_split_done: false,
            frame_count: 0,
            screenshot_saved: false,
            palette: build_palette(),
            palette_state: PaletteState::new(),
            palette_open: false,
            open_palette_arg: args.open_palette,
            open_palette_done: false,
            palette_confirm: args.palette_confirm,
            palette_confirm_done: false,
        }
    }

    fn maybe_auto_open_palette(&mut self) {
        if let Some(query) = self.open_palette_arg.clone() {
            if self.frame_count == 3 && !self.open_palette_done {
                self.palette_open = true;
                self.palette_state.set_query(query);
                self.open_palette_done = true;
            } else if self.frame_count == 8 && self.palette_confirm && !self.palette_confirm_done {
                // Same confirm()/run_palette_action() path a real Enter
                // keypress takes in handle_palette_key — verifies the
                // palette can actually *do* something, not just filter.
                if let Some(id) = self.palette_state.confirm(&mut self.palette) {
                    self.run_palette_action(&id);
                }
                self.palette_open = false;
                self.palette_confirm_done = true;
            }
        }
    }

    fn run_palette_action(&mut self, id: &str) {
        match id {
            "split-h" => {
                let _ = self.app.split_active(SplitDirection::Horizontal, 50, 15);
            }
            "split-v" => {
                let _ = self.app.split_active(SplitDirection::Vertical, 50, 15);
            }
            "close-pane" => self.app.close_active(),
            "toggle-zoom" => self.app.zoom_active(),
            "quit" => self.app.should_quit = true,
            _ => {}
        }
    }

    fn maybe_auto_split(&mut self) {
        if self.auto_split_done {
            return;
        }
        if let Some(direction) = self.auto_split {
            if self.frame_count == 3 {
                let _ = self.app.split_active(direction, 50, 15);
                self.auto_split_done = true;
            }
        }
    }

    fn maybe_auto_type(&mut self) {
        if self.auto_typed {
            return;
        }
        if let Some(text) = self.auto_type.clone() {
            if self.frame_count == 10 {
                for c in text.chars() {
                    self.app.handle_key(UtKey::Char(c), UtMods::none());
                }
                self.app.handle_key(UtKey::Enter, UtMods::none());
                self.auto_typed = true;
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.raw.events.clone());
        for event in &events {
            match event {
                egui::Event::Text(text) => {
                    if self.palette_open {
                        for c in text.chars() {
                            self.palette_state.push_char(c);
                        }
                    } else {
                        for c in text.chars() {
                            self.dispatch_key(UtKey::Char(c), UtMods::none());
                        }
                    }
                }
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    let mods = UtMods { ctrl: modifiers.ctrl, alt: modifiers.alt, shift: modifiers.shift };

                    if *key == EKey::P && mods.ctrl {
                        self.palette_open = !self.palette_open;
                        self.palette_state.clear();
                        continue;
                    }

                    if self.palette_open {
                        self.handle_palette_key(*key);
                        continue;
                    }

                    if let Some(k) = translate_key(*key) {
                        // Plain character keys arrive via Event::Text too;
                        // only forward Key-form chars when a modifier makes
                        // them meaningfully different (e.g. Ctrl+C).
                        if matches!(k, UtKey::Char(_)) && !mods.ctrl && !mods.alt {
                            continue;
                        }
                        self.dispatch_key(k, mods);
                    }
                }
                egui::Event::Screenshot { image, .. } => {
                    if let Some(path) = self.screenshot_path.clone() {
                        if !self.screenshot_saved {
                            if let Some(img) = image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, image.as_raw().to_vec()) {
                                let _ = img.save(&path);
                            }
                            self.screenshot_saved = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_palette_key(&mut self, key: EKey) {
        match key {
            EKey::Escape => self.palette_open = false,
            EKey::Backspace => self.palette_state.backspace(),
            EKey::ArrowDown => {
                let count = self.palette_state.visible_items(&self.palette).len();
                self.palette_state.move_down(count);
            }
            EKey::ArrowUp => self.palette_state.move_up(),
            EKey::Enter => {
                if let Some(id) = self.palette_state.confirm(&mut self.palette) {
                    self.run_palette_action(&id);
                }
                self.palette_open = false;
            }
            _ => {}
        }
    }

    fn dispatch_key(&mut self, key: UtKey, mods: UtMods) {
        if self.prefix_pending {
            self.prefix_pending = false;
            match key {
                UtKey::Char('"') => {
                    let _ = self.app.split_active(SplitDirection::Vertical, 50, 15);
                }
                UtKey::Char('-') => {
                    let _ = self.app.split_active(SplitDirection::Horizontal, 50, 15);
                }
                UtKey::Char('x') => self.app.close_active(),
                UtKey::Char('z') => self.app.zoom_active(),
                UtKey::Char('q') => self.app.should_quit = true,
                _ => {}
            }
            return;
        }
        if key == UtKey::Char('b') && mods.ctrl {
            self.prefix_pending = true;
            return;
        }
        self.app.handle_key(key, mods);
        if let Some(pane) = self.app.tab.panes_mut().iter_mut().find(|_| true) {
            let _ = pane; // nonblocking already set at construction
        }
    }
}

fn translate_key(key: EKey) -> Option<UtKey> {
    Some(match key {
        EKey::Enter => UtKey::Enter,
        EKey::Backspace => UtKey::Backspace,
        EKey::Delete => UtKey::Delete,
        EKey::Tab => UtKey::Tab,
        EKey::Escape => UtKey::Escape,
        EKey::ArrowUp => UtKey::Up,
        EKey::ArrowDown => UtKey::Down,
        EKey::ArrowLeft => UtKey::Left,
        EKey::ArrowRight => UtKey::Right,
        EKey::Home => UtKey::Home,
        EKey::End => UtKey::End,
        EKey::PageUp => UtKey::PageUp,
        EKey::PageDown => UtKey::PageDown,
        EKey::Insert => UtKey::Insert,
        EKey::F1 => UtKey::Function(1),
        EKey::F2 => UtKey::Function(2),
        EKey::F3 => UtKey::Function(3),
        EKey::F4 => UtKey::Function(4),
        EKey::F5 => UtKey::Function(5),
        EKey::F6 => UtKey::Function(6),
        EKey::F7 => UtKey::Function(7),
        EKey::F8 => UtKey::Function(8),
        EKey::F9 => UtKey::Function(9),
        EKey::F10 => UtKey::Function(10),
        EKey::F11 => UtKey::Function(11),
        EKey::F12 => UtKey::Function(12),
        // Letters/digits/symbols: forwarded as Char via egui's key names so
        // Ctrl-combos (which don't produce an Event::Text) still work.
        other => {
            let name = other.name();
            if name.len() == 1 {
                UtKey::Char(name.chars().next().unwrap().to_ascii_lowercase())
            } else {
                return None;
            }
        }
    })
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Newly split panes start in blocking mode; a blocking read here
        // would freeze this single-threaded UI loop entirely (this is the
        // same class of bug ut-daemon hit and fixed the same way). Cheap
        // and idempotent to re-assert every frame rather than trying to
        // catch every place a pane gets added.
        for pane in self.app.tab.panes_mut() {
            let _ = pane.set_nonblocking(true);
        }
        self.app.pump_all();
        self.handle_input(ctx);
        self.maybe_auto_split();
        self.maybe_auto_type();
        self.maybe_auto_open_palette();

        if self.char_size.is_none() {
            let font_id = FontId::monospace(self.font_size);
            let size = ctx.fonts(|f| egui::vec2(f.glyph_width(&font_id, ' '), f.row_height(&font_id)));
            self.char_size = Some(size);
        }

        let confirming = matches!(self.app.prompt, PromptState::Confirming { .. });

        egui::TopBottomPanel::top("titlebar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Zohara Terminal").strong());
                ui.separator();
                ui.label(format!("panes: {}", self.app.tab.panes().len()));
                if let Some(id) = self.app.tab.active_pane().map(|p| p.id) {
                    ui.label(format!("active: {id}"));
                }
            });
        });

        if confirming {
            self.draw_confirmation(ctx);
        }

        egui::CentralPanel::default().frame(egui::Frame::none().fill(Color32::from_gray(18))).show(ctx, |ui| {
            self.draw_panes(ui);
        });

        self.draw_palette(ctx);

        self.frame_count += 1;

        if let Some(path) = self.screenshot_path.clone() {
            let capture_frame = self.exit_after_frames.unwrap_or(20);
            if self.frame_count == capture_frame {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            if self.screenshot_saved {
                let _ = path; // already written in handle_input
                std::process::exit(0);
            }
        } else if self.app.should_quit {
            std::process::exit(0);
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

impl GuiApp {
    fn draw_confirmation(&mut self, ctx: &egui::Context) {
        let PromptState::Confirming { report_summary, .. } = &self.app.prompt else { return };
        let summary = report_summary.clone();
        egui::TopBottomPanel::bottom("confirm").min_height(80.0).show(ctx, |ui| {
            ui.colored_label(Color32::from_rgb(230, 200, 60), "Destructive command — send anyway? (y/n)");
            for line in &summary {
                ui.label(line);
            }
        });
    }

    fn draw_palette(&mut self, ctx: &egui::Context) {
        if !self.palette_open {
            return;
        }
        let items = self.palette_state.visible_items(&self.palette);
        let selected = self.palette_state.selected_index().min(items.len().saturating_sub(1));

        egui::Window::new("palette")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
            .fixed_size(egui::vec2(440.0, 260.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Command Palette");
                    ui.weak("(Ctrl+P to close, type to search, Enter to run)");
                });
                ui.separator();
                ui.label(egui::RichText::new(format!("> {}", self.palette_state.query())).monospace().size(16.0));
                ui.separator();
                egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                    if items.is_empty() {
                        ui.weak("No matching commands");
                    }
                    for (i, item) in items.iter().enumerate() {
                        let label = match &item.subtitle {
                            Some(sub) => format!("{}  —  {}", item.label, sub),
                            None => item.label.clone(),
                        };
                        let text = if i == selected {
                            egui::RichText::new(format!("▶ {label}")).color(Color32::from_rgb(100, 200, 220)).strong()
                        } else {
                            egui::RichText::new(format!("  {label}")).color(Color32::from_gray(200))
                        };
                        ui.label(text);
                    }
                });
            });
    }

    fn draw_panes(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let active_id = self.app.tab.active_pane().map(|p| p.id);
        let effective = self.app.layout.effective_layout();
        let bounds = URect { x: 0.0, y: 0.0, w: avail.width(), h: avail.height() };
        let char_size = self.char_size.unwrap_or(egui::vec2(8.0, 16.0));
        let font_size = self.font_size;

        for (pane_id, rect) in effective.rects(bounds) {
            let screen_rect = egui::Rect::from_min_size(avail.min + egui::vec2(rect.x, rect.y), egui::vec2(rect.w, rect.h));

            let cols = ((rect.w - 12.0) / char_size.x).floor().max(10.0) as u16;
            let rows = ((rect.h - 30.0) / char_size.y).floor().max(3.0) as u16;
            if self.last_pane_sizes.get(&pane_id) != Some(&(cols, rows)) {
                if let Some(pane) = self.app.tab.panes_mut().iter_mut().find(|p| p.id == pane_id) {
                    let _ = pane.resize(cols, rows);
                }
                self.last_pane_sizes.insert(pane_id, (cols, rows));
            }

            let focused = Some(pane_id) == active_id;
            let stroke_color = if focused { Color32::from_rgb(100, 200, 220) } else { Color32::from_gray(70) };

            ui.scope_builder(egui::UiBuilder::new().max_rect(screen_rect), |ui| {
                egui::Frame::none()
                    .stroke(egui::Stroke::new(if focused { 2.0 } else { 1.0 }, stroke_color))
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.set_min_size(screen_rect.size() - egui::vec2(12.0, 12.0));
                        ui.label(egui::RichText::new(format!("pane {pane_id}")).small().color(Color32::from_gray(150)));
                        if let Some(terminal) = self.app.grid_for(pane_id) {
                            let job = crate::render::grid_to_layout_job(terminal.grid(), font_size);
                            ui.label(job);
                        }
                    });
            });
        }
    }
}

mod render {
    pub use ut_gui::render::grid_to_layout_job;
}
