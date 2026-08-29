//! Terminal widget: alacritty_terminal grid model rendered with egui.
//!
//! Byte flow: PTY output -> `feed()` -> vte processor -> Term grid -> `paint()`.
//! Input flow: egui keyboard/IME events -> `encode_key`/UTF-8 bytes -> session input.
//! Terminal query responses (cursor position report, DA1, etc.) arrive as
//! `Event::PtyWrite` from the model and are routed back through the same input
//! channel — this is what keeps vim/tmux working.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor, Rgb};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Event proxy: model events -> GUI / PTY
// ---------------------------------------------------------------------------

/// Cloneable sink for events emitted by the terminal model while feeding bytes.
#[derive(Clone)]
pub struct EventProxy {
    /// Terminal-initiated writes back to the PTY (CPR, DA responses, bracketed
    /// paste responses, kitty keyboard queries...)
    pub input_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub title_tx: mpsc::UnboundedSender<String>,
    pub bell_tx: mpsc::UnboundedSender<()>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                let _ = self.input_tx.send(text.into_bytes());
            }
            Event::Title(title) => {
                let _ = self.title_tx.send(title);
            }
            Event::Bell => {
                let _ = self.bell_tx.send(());
            }
            Event::ColorRequest(index, format) => {
                let rgb = default_palette(index);
                let _ = self.input_tx.send(format(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                // Cell size in pixels is filled in by the widget before feeding;
                // a nominal size is fine here (only exotic TUIs use it).
                let _ = self.input_tx.send(
                    format(alacritty_terminal::event::WindowSize {
                        num_lines: 24,
                        num_cols: 80,
                        cell_width: 8,
                        cell_height: 16,
                    })
                    .into_bytes(),
                );
            }
            Event::ClipboardStore(_ctype, text) => {
                // OSC52 clipboard store: best effort via title channel abuse is wrong;
                // ignore here (the UI reads the clipboard itself on selection).
                let _ = text;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal state (one per session tab)
// ---------------------------------------------------------------------------

struct Dims {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for Dims {
    fn columns(&self) -> usize {
        self.columns
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
}

/// Selection endpoints in absolute grid coordinates (Line may be negative history)
#[derive(Clone, Copy)]
struct SelPoint {
    line: i32,
    col: usize,
}

pub struct Terminal {
    pub term: Term<EventProxy>,
    parser: Processor,
    /// User input / terminal responses go here (to the session task)
    input_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Window/tab title set by the remote (OSC 0/2)
    pub title: String,
    /// BEL seen since last paint
    pub bell: bool,
    /// Cell metrics from the last paint (pixels)
    cell_w: f32,
    cell_h: f32,
    /// Terminal size in cells
    cols: u16,
    rows: u16,
    selection: Option<(SelPoint, SelPoint)>,
    drag_anchor: Option<SelPoint>,
    /// Whether we enabled IME for the window (terminal focused)
    ime_allowed: bool,
    /// True while an IME composition is in progress (pinyin being edited):
    /// raw key presses in this window must NOT reach the remote, or editing
    /// the pinyin (backspace etc.) would delete text already typed.
    composing: bool,
}

impl Terminal {
    /// Create a terminal; also returns receivers for the model-emitted title
    /// (OSC) and bell (BEL) events.
    #[allow(clippy::type_complexity)]
    pub fn new(
        cols: u16,
        rows: u16,
        scrollback: u32,
        input_tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> (Self, mpsc::UnboundedReceiver<String>, mpsc::UnboundedReceiver<()>) {
        let (title_tx, title_rx) = mpsc::unbounded_channel();
        let (bell_tx, bell_rx) = mpsc::unbounded_channel();
        let proxy = EventProxy { input_tx: input_tx.clone(), title_tx, bell_tx };
        let mut config = TermConfig::default();
        config.scrolling_history = scrollback as usize;
        let term = Term::new(config, &Dims { columns: cols as usize, screen_lines: rows as usize }, proxy);
        (
            Self {
                term,
                parser: Processor::new(),
                input_tx,
                title: String::new(),
                bell: false,
                cell_w: 8.0,
                cell_h: 16.0,
                cols,
                rows,
                selection: None,
                drag_anchor: None,
                ime_allowed: false,
                composing: false,
            },
            title_rx,
            bell_rx,
        )
    }

    /// Feed raw PTY output through the VT processor
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resize the grid; returns true if the size actually changed
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if cols == self.cols && rows == self.rows || cols == 0 || rows == 0 {
            return false;
        }
        self.term.resize(Dims { columns: cols as usize, screen_lines: rows as usize });
        self.cols = cols;
        self.rows = rows;
        self.selection = None;
        true
    }

    /// Send user input bytes to the PTY
    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.input_tx.send(bytes.to_vec());
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Current grid size in cells (set by paint/resize)
    pub fn grid_size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    // -- selection ---------------------------------------------------------

    fn point_from_pos(&self, pos: egui::Pos2, origin: egui::Pos2) -> SelPoint {
        let col = (((pos.x - origin.x) / self.cell_w).floor() as i32)
            .clamp(0, self.cols as i32 - 1) as usize;
        let row = (((pos.y - origin.y) / self.cell_h).floor() as i32)
            .clamp(0, self.rows as i32 - 1) as i32;
        SelPoint { line: row - self.display_offset() as i32, col }
    }

    /// Extract the selected text in reading order
    fn selection_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = if (a.line, a.col) <= (b.line, b.col) { (a, b) } else { (b, a) };
        let grid = self.term.grid();
        let mut out = String::new();
        for line in start.line..=end.line {
            let row = &grid[Line(line)];
            let col_end = if line == end.line { end.col.min(grid.columns() - 1) } else { grid.columns() - 1 };
            let col_start = if line == start.line { start.col } else { 0 };
            let mut line_text = String::new();
            for col in col_start..=col_end {
                let cell = &row[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                if cell.c != ' ' || cell.flags.contains(Flags::WIDE_CHAR) {
                    line_text.push(cell.c);
                } else {
                    line_text.push(' ');
                }
            }
            // Trim trailing spaces (common when selecting the end of lines)
            let trimmed = line_text.trim_end_matches(' ');
            out.push_str(trimmed);
            if line != end.line {
                out.push('\n');
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    // -- painting ----------------------------------------------------------

    /// Paint the terminal into `ui`. Returns true when the grid size changed
    /// (the caller must then forward the new size to the remote PTY).
    pub fn paint(
        &mut self,
        ui: &mut egui::Ui,
        font_size: f32,
        copy_on_select: bool,
    ) -> bool {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(
                ui.available_width(),
                ui.available_height(),
            ),
            egui::Sense::click() | egui::Sense::drag(),
        );
        let painter = ui.painter_at(rect);
        let origin = rect.min;

        // Cell metrics from the monospace font
        let font_id = egui::FontId::monospace(font_size);
        let cell_w = ui
            .ctx()
            .fonts_mut(|f| f.glyph_width(&font_id, 'M'))
            .max(1.0);
        let cell_h = font_size * 1.25;
        self.cell_w = cell_w;
        self.cell_h = cell_h;

        // Compute grid size and resize both local model and remote PTY
        let cols = ((rect.width() / cell_w).floor() as u16).max(2);
        let rows = ((rect.height() / cell_h).floor() as u16).max(2);
        let resized = self.resize(cols, rows);

        let grid = self.term.grid();
        let offset = grid.display_offset();
        let default_fg = self.resolve_color(Color::Named(NamedColor::Foreground), ui);
        let default_bg = self.resolve_color(Color::Named(NamedColor::Background), ui);
        painter.rect_filled(rect, 0.0, default_bg);

        let bold_id = egui::FontId::new(font_size, egui::FontFamily::Monospace);

        for row_i in 0..self.rows as i32 {
            let line_abs = row_i - offset as i32;
            let grid_row = &grid[Line(line_abs)];
            let y = origin.y + row_i as f32 * cell_h;

            // Render as runs of same-style cells to limit paint calls
            let mut run = String::new();
            let mut run_col = 0usize;
            let mut run_style: Option<(egui::Color32, egui::Color32, bool)> = None;

            let flush = |run: &mut String,
                         run_col: &mut usize,
                         run_style: &mut Option<(egui::Color32, egui::Color32, bool)>,
                         end_col: usize,
                         painter: &egui::Painter,
                         y: f32| {
                if let Some((fg, bg, underline)) = *run_style {
                    if !run.is_empty() {
                        let x = origin.x + *run_col as f32 * cell_w;
                        let w = (end_col - *run_col).max(1) as f32 * cell_w;
                        if bg != default_bg {
                            painter.rect_filled(
                                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, cell_h)),
                                0.0,
                                bg,
                            );
                        }
                        painter.text(
                            egui::pos2(x, y + cell_h * 0.5),
                            egui::Align2::LEFT_CENTER,
                            run.as_str(),
                            bold_id.clone(),
                            fg,
                        );
                        if underline {
                            painter.line_segment(
                                [
                                    egui::pos2(x, y + cell_h - 1.0),
                                    egui::pos2(x + w, y + cell_h - 1.0),
                                ],
                                egui::Stroke::new(1.0, fg),
                            );
                        }
                    }
                }
                run.clear();
                *run_col = end_col;
                *run_style = None;
            };

            for col_i in 0..self.cols as usize {
                let cell = &grid_row[Column(col_i)];
                let wide = cell.flags.contains(Flags::WIDE_CHAR);
                let spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
                let ch = if spacer { continue } else { cell.c };

                let fg = self.resolve_color(cell.fg, ui);
                let mut bg = self.resolve_color(cell.bg, ui);
                let mut fg = fg;
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let underline = cell.flags.intersects(Flags::UNDERLINE);

                let style = (fg, bg, underline);
                let style_changed = match run_style {
                    None => {
                        run_style = Some(style);
                        false
                    }
                    Some(s) => s != style,
                };
                if style_changed {
                    flush(&mut run, &mut run_col, &mut run_style, col_i, &painter, y);
                    run_style = Some(style);
                }
                if wide {
                    // Wide char: render alone spanning two cells
                    let had = run_style.take();
                    flush(&mut run, &mut run_col, &mut run_style, col_i, &painter, y);
                    let x = origin.x + col_i as f32 * cell_w;
                    if bg != default_bg {
                        painter.rect_filled(
                            egui::Rect::from_min_size(
                                egui::pos2(x, y),
                                egui::vec2(cell_w * 2.0, cell_h),
                            ),
                            0.0,
                            bg,
                        );
                    }
                    painter.text(
                        egui::pos2(x, y + cell_h * 0.5),
                        egui::Align2::LEFT_CENTER,
                        ch.to_string(),
                        bold_id.clone(),
                        fg,
                    );
                    run_style = had;
                } else {
                    run.push(ch);
                }
            }
            flush(&mut run, &mut run_col, &mut run_style, self.cols as usize, &painter, y);
        }

        // Selection highlight overlay
        if let Some((a, b)) = self.selection {
            let (start, end) = if (a.line, a.col) <= (b.line, b.col) { (a, b) } else { (b, a) };
            for row_i in 0..self.rows as i32 {
                let line_abs = row_i - offset as i32;
                if line_abs < start.line || line_abs > end.line {
                    continue;
                }
                let col_start = if line_abs == start.line { start.col } else { 0 };
                let col_end = if line_abs == end.line { end.col + 1 } else { self.cols as usize };
                let x = origin.x + col_start as f32 * cell_w;
                let w = ((col_end - col_start) as f32 * cell_w).min(rect.max.x - x);
                if w > 0.0 {
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(x, origin.y + row_i as f32 * cell_h),
                            egui::vec2(w, cell_h),
                        ),
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(120, 160, 255, 90),
                    );
                }
            }
        }

        // Cursor (only on the visible screen when not scrolled into history)
        if offset == 0 {
            let cp = grid.cursor.point;
            if (cp.line.0 as usize) < self.rows as usize && (cp.column.0 as usize) < self.cols as usize {
                let x = origin.x + cp.column.0 as f32 * cell_w;
                let y = origin.y + cp.line.0 as f32 * cell_h;
                let stroke = if response.has_focus() {
                    egui::Stroke::NONE
                } else {
                    egui::Stroke::new(1.0, default_fg)
                };
                let color = if response.has_focus() {
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 110)
                } else {
                    egui::Color32::TRANSPARENT
                };
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, cell_h)),
                    0.0,
                    color,
                );
                if stroke != egui::Stroke::NONE {
                    painter.rect_stroke(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, cell_h)),
                        0.0,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        // ---- input handling -------------------------------------------------
        if response.clicked() {
            response.request_focus();
        }

        // Mouse selection
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                self.drag_anchor = Some(self.point_from_pos(pos, origin));
                self.selection = None;
            }
        } else if response.dragged() {
            if let (Some(anchor), Some(pos)) = (self.drag_anchor, response.interact_pointer_pos()) {
                let cur = self.point_from_pos(pos, origin);
                self.selection = Some((anchor, cur));
            }
        } else if response.drag_stopped() {
            if copy_on_select {
                if let Some(text) = self.selection_text() {
                    ui.ctx().copy_text(text);
                }
            }
        }
        // Double-click selects the word under the pointer (simple whitespace split)
        if response.double_clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let p = self.point_from_pos(pos, origin);
                self.selection = Some((p, p));
                if let Some(text) = self.selection_text() {
                    ui.ctx().copy_text(text);
                }
            }
        }

        // Scroll wheel: history scroll, or arrow keys in alternate screen (less/tmux)
        let mode = self.term.mode().clone();
        if response.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.5 {
                let lines = (scroll / cell_h).round() as i32;
                if mode.contains(TermMode::ALT_SCREEN) {
                    for _ in 0..lines.abs().min(10) {
                        let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
                        self.write(seq);
                    }
                } else if lines != 0 {
                    self.term.scroll_display(Scroll::Delta(-lines));
                }
            }
        }

        // IME: enable composition while the terminal has focus, and park the
        // composition window at the terminal cursor position.
        let focused = response.has_focus();
        if focused != self.ime_allowed {
            self.ime_allowed = focused;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::IMEAllowed(focused));
        }
        if focused {
            let cp = self.term.grid().cursor.point;
            let ime_rect = egui::Rect::from_min_size(
                egui::pos2(
                    origin.x + cp.column.0 as f32 * cell_w,
                    origin.y + (cp.line.0 as f32 - offset as f32) * cell_h,
                ),
                egui::vec2(cell_w * 8.0, cell_h),
            );
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::IMERect(ime_rect));
        }

        // Keyboard input when focused
        if focused {
            let events = ui.input(|i| i.events.clone());
            // Pre-pass: IME state first. Within a frame, key events arrive BEFORE
            // the Preedit update for the same keystroke, so handling them in order
            // would leak the first key of every composition.
            for ev in &events {
                if let egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) = ev {
                    self.composing = !text.is_empty();
                }
            }
            for ev in events {
                match ev {
                    egui::Event::Ime(ime) => match ime {
                        egui::ImeEvent::Preedit { text, .. } => {
                            // Composition in progress: swallow raw keys until Commit
                            self.composing = !text.is_empty();
                        }
                        egui::ImeEvent::Commit(text) => {
                            // IME composition finished (Chinese/Japanese/Korean input)
                            self.composing = false;
                            let bytes = text
                                .chars()
                                .filter(|&c| c >= ' ')
                                .collect::<String>()
                                .into_bytes();
                            if !bytes.is_empty() {
                                self.write(&bytes);
                            }
                        }
                        _ => {}
                    },
                    // While the user is editing pinyin, winit still leaks the raw
                    // key events to us (egui only filters them when a TextEdit has
                    // focus) — dropping everything keeps the remote untouched.
                    egui::Event::Key { .. } if self.composing => {}
                    egui::Event::Text(_) if self.composing => {}
                    egui::Event::Text(text) => {
                        // Printable / IME-committed text
                        let bytes = text
                            .chars()
                            .filter(|&c| c >= ' ')
                            .collect::<String>()
                            .into_bytes();
                        if !bytes.is_empty() {
                            self.write(&bytes);
                        }
                    }
                    egui::Event::Key {
                        key,
                        modifiers,
                        pressed: true,
                        ..
                    } => {
                        // Copy/paste shortcuts
                        if modifiers.ctrl && modifiers.shift {
                            match key {
                                egui::Key::C => {
                                    if let Some(text) = self.selection_text() {
                                        ui.ctx().copy_text(text);
                                    }
                                    continue;
                                }
                                egui::Key::V => {
                                    // winit delivers Ctrl+V as an Event::Paste below
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if let Some(bytes) = encode_key(key, &modifiers, &mode) {
                            self.write(&bytes);
                        }
                    }
                    egui::Event::Paste(text) => {
                        self.paste(&text, &mode);
                    }
                    _ => {}
                }
            }
        }
        resized
    }

    fn paste(&mut self, text: &str, mode: &TermMode) {
        if text.is_empty() {
            return;
        }
        if mode.contains(TermMode::BRACKETED_PASTE) {
            let mut bytes = b"\x1b[200~".to_vec();
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            self.write(&bytes);
        } else {
            // Strip newlines to avoid accidental command execution on paste
            let sanitized = text.replace("\r", "").replace("\n", "");
            self.write(sanitized.as_bytes());
        }
    }

    /// Resolve a model color to an egui color using the default dark palette
    /// plus per-terminal overrides from OSC 4/10/11.
    fn resolve_color(&self, color: Color, _ui: &egui::Context) -> egui::Color32 {
        let overridden: Option<Rgb> = match color {
            Color::Named(n) => self.term.colors()[n],
            Color::Indexed(i) => self.term.colors()[i as usize],
            Color::Spec(_) => None,
        };
        let rgb = overridden.unwrap_or_else(|| match color {
            Color::Spec(rgb) => rgb,
            Color::Named(n) => default_palette(match n {
                NamedColor::Foreground => 256,
                NamedColor::Background => 257,
                NamedColor::Cursor => 258,
                other => other as usize,
            }),
            Color::Indexed(i) => default_palette(i as usize),
        });
        egui::Color32::from_rgb(rgb.r, rgb.g, rgb.b)
    }
}

/// xterm-256 default palette; indices 256/257/258 are fg/bg/cursor
fn default_palette(index: usize) -> Rgb {
    let c = |r: u8, g: u8, b: u8| Rgb { r, g, b };
    match index {
        256 => c(0xd4, 0xd4, 0xd4), // foreground
        257 => c(0x1e, 0x1e, 0x1e), // background
        258 => c(0xd4, 0xd4, 0xd4), // cursor
        0 => c(0x00, 0x00, 0x00),
        1 => c(0xcd, 0x31, 0x31),
        2 => c(0x00, 0xcd, 0x00),
        3 => c(0xcd, 0xcd, 0x00),
        4 => c(0x00, 0x91, 0xff),
        5 => c(0xcd, 0x00, 0xcd),
        6 => c(0x00, 0xcd, 0xcd),
        7 => c(0xe5, 0xe5, 0xe5),
        8 => c(0x7f, 0x7f, 0x7f),
        9 => c(0xff, 0x00, 0x00),
        10 => c(0x00, 0xff, 0x00),
        11 => c(0xff, 0xff, 0x00),
        12 => c(0x00, 0x00, 0xff),
        13 => c(0xff, 0x00, 0xff),
        14 => c(0x00, 0xff, 0xff),
        15 => c(0xff, 0xff, 0xff),
        i @ 16..=231 => {
            // 6x6x6 color cube
            let i = i - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            c(
                steps[(i / 36) % 6],
                steps[(i / 6) % 6],
                steps[i % 6],
            )
        }
        i @ 232..=255 => {
            // Grayscale ramp
            let v: u8 = (8 + (i - 232) * 10) as u8;
            c(v, v, v)
        }
        _ => c(0xd4, 0xd4, 0xd4),
    }
}

/// Convert an egui key press to the bytes a real terminal would send.
/// `mode` decides application-cursor / application-keypad variants.
fn encode_key(key: egui::Key, mods: &egui::Modifiers, mode: &TermMode) -> Option<Vec<u8>> {
    use egui::Key;
    let esc: &[u8] = b"\x1b";
    let app = mode.contains(TermMode::APP_CURSOR);
    // Ctrl+letter -> control code (but leave Ctrl+Shift to the widget above)
    if mods.ctrl && !mods.shift {
        if let Some(c) = match key {
            Key::A => Some('a'),
            Key::B => Some('b'),
            Key::C => Some('c'),
            Key::D => Some('d'),
            Key::E => Some('e'),
            Key::F => Some('f'),
            Key::G => Some('g'),
            Key::H => Some('h'),
            Key::K => Some('k'),
            Key::L => Some('l'),
            Key::N => Some('n'),
            Key::P => Some('p'),
            Key::T => Some('t'),
            Key::U => Some('u'),
            Key::W => Some('w'),
            Key::X => Some('x'),
            Key::Y => Some('y'),
            Key::Z => Some('z'),
            _ => None,
        } {
            return Some(vec![c as u8 - b'a' + 1]);
        }
    }
    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        Key::Escape => esc.to_vec(),
        Key::ArrowUp => {
            if app { b"\x1bOA".to_vec() } else { b"\x1b[A".to_vec() }
        }
        Key::ArrowDown => {
            if app { b"\x1bOB".to_vec() } else { b"\x1b[B".to_vec() }
        }
        Key::ArrowRight => {
            if app { b"\x1bOC".to_vec() } else { b"\x1b[C".to_vec() }
        }
        Key::ArrowLeft => {
            if app { b"\x1bOD".to_vec() } else { b"\x1b[D".to_vec() }
        }
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => b"\x1b[15~".to_vec(),
        Key::F6 => b"\x1b[17~".to_vec(),
        Key::F7 => b"\x1b[18~".to_vec(),
        Key::F8 => b"\x1b[19~".to_vec(),
        Key::F9 => b"\x1b[20~".to_vec(),
        Key::F10 => b"\x1b[21~".to_vec(),
        Key::F11 => b"\x1b[23~".to_vec(),
        Key::F12 => b"\x1b[24~".to_vec(),
        _ => return None,
    })
}
