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

/// Diagnostic cursor/IO log. Off unless XXSSHG_DEBUG_CURSOR=1
/// (log: ~/.xxssh/cursor-debug.log).
pub fn diag_log(msg: &str) {
    use std::io::Write;
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("XXSSHG_DEBUG_CURSOR").ok().as_deref() == Some("1")) {
        return;
    }
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".xxssh").join("cursor-debug.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{msg}");
        }
    }
}

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
    /// Current IME composition text, rendered inline at the terminal cursor
    /// (like a real terminal: the user sees the pinyin while typing it)
    preedit: String,
    /// Ctrl+= / Ctrl+- pressed (zoom direction); consumed by the app after paint
    pub pending_zoom: Option<f32>,
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
                preedit: String::new(),
                pending_zoom: None,
            },
            title_rx,
            bell_rx,
        )
    }

    /// Feed raw PTY output through the VT processor
    pub fn feed(&mut self, bytes: &[u8]) {
        let head: String = bytes.iter().take(24).map(|&b| format!("{:02x} ", b)).collect();
        diag_log(&format!(
            "feed: {} bytes head: {}",
            bytes.len(),
            head
        ));
        self.parser.advance(&mut self.term, bytes);
        let cp = self.term.grid().cursor.point;
        diag_log(&format!("feed-done: cursor=({},{})", cp.line.0, cp.column.0));
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
        invert_scrolling: bool,
    ) -> bool {
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(
                ui.available_width(),
                ui.available_height(),
            ),
            egui::Sense::click() | egui::Sense::drag(),
        );
        // Cell metrics from the monospace font
        let font_id = egui::FontId::monospace(font_size);
        // Snap cell metrics and the grid origin to whole physical pixels:
        // fractional glyph positions (measured widths are fractional, vertical
        // centering adds 0.5px) make stems blurry / ghosted on LCDs.
        let ppp = ui.ctx().pixels_per_point();
        let q = |v: f32| (v * ppp).round() / ppp;
        // Measure the real typographic ADVANCE (layout of 10 'M's / 10), not the
        // glyph ink width: glyph_width('M') is the bounding-box width, which lacks
        // the side bearings, so text drifts right cumulatively while the cursor
        // block stays on its grid column — the cursor appears to lag behind.
        let cell_w = q(
            ui.ctx()
                .fonts_mut(|f| {
                    f.layout_no_wrap("MMMMMMMMMM".to_string(), font_id.clone(), egui::Color32::WHITE)
                        .rect
                        .width()
                        / 10.0
                })
                .max(1.0),
        )
        .max(1.0);
        // Use egui's own recommended row height for the font: a fixed 1.25x guess
        // drifts from the real glyph metrics as the font size changes, making rows
        // overlap (ghosting) and the cursor block sit misaligned.
        let cell_h = q(ui.ctx().fonts_mut(|f| f.row_height(&font_id)).max(font_size)).max(1.0);
        let origin = egui::pos2(q(rect.min.x), q(rect.min.y));
        self.cell_w = cell_w;
        self.cell_h = cell_h;

        // Headroom for the first row's ascenders (tall CJK glyphs at the top
        // edge would otherwise be clipped by the widget rect)

        let painter = ui.painter_at(rect.expand2(egui::vec2(0.0, cell_h * 0.35)));

        // Compute grid size and resize both local model and remote PTY
        let cols = ((rect.width() / cell_w).floor() as u16).max(2);
        let rows = ((rect.height() / cell_h).floor() as u16).max(2);
        let resized = self.resize(cols, rows);

        let grid = self.term.grid();
        let offset = grid.display_offset();
        let default_fg = self.resolve_color(Color::Named(NamedColor::Foreground), ui);
        let default_bg = self.resolve_color(Color::Named(NamedColor::Background), ui);
        painter.rect_filled(rect, 0.0, default_bg);

        let mono_id = egui::FontId::new(font_size, egui::FontFamily::Monospace);

        // Render PER CELL at absolute grid coordinates. Batching runs through the
        // text layouter measures advances slightly differently than the grid steps,
        // and any sub-pixel mismatch accumulates along the line — the cursor block
        // (also positioned on the grid) would visibly lag behind the text.
        for row_i in 0..self.rows as i32 {
            let line_abs = row_i - offset as i32;
            let grid_row = &grid[Line(line_abs)];
            let y = q(origin.y + row_i as f32 * cell_h);

            for col_i in 0..self.cols as usize {
                let cell = &grid_row[Column(col_i)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let wide = cell.flags.contains(Flags::WIDE_CHAR);
                let ch = cell.c;
                if ch == ' ' && !cell.flags.contains(Flags::INVERSE) && !wide {
                    continue; // blank default cell: nothing to draw
                }

                let fg = self.resolve_color(cell.fg, ui);
                let mut bg = self.resolve_color(cell.bg, ui);
                let mut fg = fg;
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let underline = cell.flags.intersects(Flags::UNDERLINE);

                let x = q(origin.x + col_i as f32 * cell_w);
                let cells = if wide { 2.0 } else { 1.0 };
                let w = cell_w * cells;

                if bg != default_bg || cell.flags.contains(Flags::INVERSE) {
                    painter.rect_filled(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, cell_h)),
                        0.0,
                        bg,
                    );
                }
                // CJK glyphs use the fallback font whose baseline differs from
                // the Latin mono font; apply the precomputed em-fraction shift
                // (see fonts::compute_baseline_shift) so baselines coincide.
                let y_draw = if wide {
                    match crate::fonts::CJK_BASELINE_SHIFT_EM.get() {
                        Some(shift_em) => y + shift_em * font_size,
                        None => y,
                    }
                } else {
                    y
                };
                painter.text(
                    egui::pos2(x, q(y_draw)),
                    egui::Align2::LEFT_TOP,
                    ch.to_string(),
                    mono_id.clone(),
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

        // IME composition rendered inline at the cursor (like a real terminal):
        // the pinyin is buffer-local — it only reaches the remote on commit.
        if !self.preedit.is_empty() && offset == 0 {
            let cp = grid.cursor.point;
            if (cp.line.0 as usize) < self.rows as usize {
                let px = origin.x + cp.column.0 as f32 * cell_w;
                let py = origin.y + cp.line.0 as f32 * cell_h;
                let w = self.preedit.chars().count() as f32 * cell_w;
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(px, py), egui::vec2(w.max(cell_w), cell_h)),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(70, 110, 190, 120),
                );
                painter.text(
                    egui::pos2(px, q(py)),
                    egui::Align2::LEFT_TOP,
                    &self.preedit,
                    mono_id.clone(),
                    egui::Color32::WHITE,
                );
            }
        }

        // Cursor (only on the visible screen when not scrolled into history)
        {
            let cp0 = grid.cursor.point;
            diag_log(&format!(
                "paint: cursor=({},{}), offset={}, grid={}x{}, cell={}x{}, origin=({:.1},{:.1}), ppp={:.2}, font={:.1}",
                cp0.line.0, cp0.column.0, offset, self.rows, self.cols, cell_w, cell_h, origin.x, origin.y, ppp, font_size
            ));
        }
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
                // The block covers the glyph's em box (1.0x font size), not the
                // whole line box: text is TOP-aligned so the extra leading below
                // the glyphs would otherwise make the cursor sit visually low.
                let block_h = q(font_size);
                painter.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, block_h)),
                    0.0,
                    color,
                );
                if stroke != egui::Stroke::NONE {
                    painter.rect_stroke(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, block_h)),
                        0.0,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }

        // ---- input handling -------------------------------------------------
        // Right-click: copy when there is a selection, paste when there is none.
        // Plain left-click always clears an existing selection.
        if response.secondary_clicked() {
            let mode = self.term.mode().clone();
            if let Some(text) = self.selection_text() {
                ui.ctx().copy_text(text);
                self.selection = None;
            } else if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    if !text.is_empty() {
                        self.paste(&text, &mode);
                    }
                }
            }
        }
        if response.clicked() {
            response.request_focus();
            self.selection = None;
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
                let dir: i32 = if invert_scrolling { -1 } else { 1 };
                let lines = ((scroll / cell_h).round() as i32) * dir;
                if mode.contains(TermMode::ALT_SCREEN) {
                    for _ in 0..lines.abs().min(10) {
                        let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
                        self.write(seq);
                    }
                } else if lines != 0 {
                    self.term.scroll_display(Scroll::Delta(lines));
                }
            }
        }

        // IME: enable composition while the terminal has focus, and park the
        // composition window at the terminal cursor position.
        let focused = response.has_focus();
        // Claim exclusive access to Tab / arrows / Escape while focused, so egui
        // does not use them for focus navigation (Tab used to jump to the sidebar)
        if focused {
            ui.memory_mut(|mem| {
                mem.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }
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
                egui::vec2(
                    (self.preedit.chars().count().max(8)) as f32 * cell_w,
                    cell_h,
                ),
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
                    self.preedit = text.clone();
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
                            // IME composition finished: Space commits the converted
                            // CJK text, Enter commits the raw pinyin letters.
                            self.composing = false;
                            self.preedit.clear();
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
                        // Ctrl+= / Ctrl+- : font zoom (Ctrl+0 resets to 14)
                        if modifiers.ctrl && !modifiers.shift {
                            match key {
                                egui::Key::Plus | egui::Key::Equals => {
                                    self.pending_zoom = Some(1.0);
                                    continue;
                                }
                                egui::Key::Minus => {
                                    self.pending_zoom = Some(-1.0);
                                    continue;
                                }
                                egui::Key::Num0 => {
                                    self.pending_zoom = Some(f32::NAN); // NaN = reset
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        // NB: plain Ctrl+C / Ctrl+X / Ctrl+V never arrive here —
                        // egui-winit converts them to Event::Copy / Cut / Paste below.
                        if let Some(bytes) = encode_key(key, &modifiers, &mode) {
                            self.write(&bytes);
                        }
                    }
                    // egui-winit turns Ctrl+C into Copy and Ctrl+X into Cut BEFORE the
                    // key reaches us. Windows Terminal convention: with a selection
                    // they copy (and clear it); without, they send the real control
                    // code so ^C can still interrupt remote commands.
                    egui::Event::Copy => {
                        if let Some(text) = self.selection_text() {
                            ui.ctx().copy_text(text);
                            self.selection = None;
                        } else {
                            self.write(&[0x03]);
                        }
                    }
                    egui::Event::Cut => {
                        if let Some(text) = self.selection_text() {
                            ui.ctx().copy_text(text);
                            self.selection = None;
                        } else {
                            self.write(&[0x18]);
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
        256 => c(0xe8, 0xe8, 0xe8), // foreground
        257 => c(0x1e, 0x1e, 0x1e), // background
        258 => c(0xe8, 0xe8, 0xe8), // cursor
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
        _ => c(0xe8, 0xe8, 0xe8),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_grid(t: &Terminal) -> String {
        let grid = t.term.grid();
        let mut out = String::new();
        for row in 0..t.rows as i32 {
            let line_abs = row - t.display_offset() as i32;
            let grid_row = &grid[Line(line_abs)];
            for col in 0..t.cols as usize {
                let cell = &grid_row[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                out.push(cell.c);
            }
            out.push('\n');
        }
        out
    }

    /// The motd+prompt byte stream as the real server sends it must end up in the grid
    #[test]
    fn prompt_survives_feed_and_resize() {
        let (input_tx, _rx) = mpsc::unbounded_channel();
        let (mut t, _title, _bell) = Terminal::new(80, 24, 10000, input_tx);
        t.feed(b"Last login: Sat Aug 29 19:07:01 2026 from 1.2.3.4\r\n\r\n");
        t.feed(b"\x1b[?2004h\x1b]0;root@ecs-6b17-xhw01: ~\x07root@ecs-6b17-xhw01:~# ");
        let text = dump_grid(&t);
        assert!(text.contains("Last login"), "motd line missing: {text:?}");
        assert!(text.contains("root@ecs-6b17-xhw01:~#"), "PROMPT MISSING: {text:?}");

        // ... and it must still be there after the GUI's first-paint resize
        t.resize(120, 30);
        let text = dump_grid(&t);
        assert!(text.contains("root@ecs-6b17-xhw01:~#"), "prompt lost after resize: {text:?}");
    }

    /// Replays the exact byte stream captured from a real GUI session (XXSSHG_PTY_DUMP).
    /// Run with XXSSHG_PTY_TEST=<file> --nocapture to dump the resulting grid.
    #[test]
    fn replay_captured_stream() {
        let Ok(path) = std::env::var("XXSSHG_PTY_TEST") else { return };
        let bytes = std::fs::read(&path).unwrap();
        let (input_tx, _rx) = mpsc::unbounded_channel();
        let (mut t, _title, _bell) = Terminal::new(80, 24, 10000, input_tx);
        // Split at the SIGWINCH redraw (^M ^[[K ^M) to reproduce the real GUI
        // ordering: font change resizes the grid FIRST, then the redraw arrives.
        let marker = b"\r\x1b[K\r".to_vec();
        let split = bytes
            .windows(marker.len())
            .rposition(|w| w == marker.as_slice());
        if let Some(pos) = split {
            t.feed(&bytes[..pos]);
            t.resize(70, 20); // the font-size change
            t.feed(&bytes[pos..]);
        } else {
            // incremental feed with cursor trace
            let mut pos = 0usize;
            let mut last = (99i32, 999usize);
            while pos < bytes.len() {
                let end = (pos + 8).min(bytes.len());
                t.feed(&bytes[pos..end]);
                pos = end;
                let cp = t.term.grid().cursor.point;
                if (cp.line.0, cp.column.0) != last {
                    println!(
                        "after {:4} bytes [..{:02x} {:02x}]: cursor=({},{})",
                        pos,
                        bytes[pos.saturating_sub(1)],
                        *bytes.get(pos).unwrap_or(&0),
                        cp.line.0,
                        cp.column.0
                    );
                    last = (cp.line.0, cp.column.0);
                }
            }
        }
        t.resize(118, 33);
        let text = dump_grid(&t);
        println!("=== GRID DUMP ===
{text}=== END ===");
        {
            let cp = t.term.grid().cursor.point;
            let grid = t.term.grid();
            let mut last = String::new();
            for row in 0..t.rows as i32 {
                let grid_row = &grid[Line(row - t.display_offset() as i32)];
                let mut line = String::new();
                for col in 0..t.cols as usize { line.push(grid_row[Column(col)].c); }
                if line.trim().len() > 4 { last = line; }
            }
            println!("CURSOR line={} col={} | last: {:?}", cp.line.0, cp.column.0, last.trim_end());
        }
        assert!(text.contains("root@ecs-6b17-xhw01:~#"), "PROMPT MISSING from replayed stream");
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    fn dump(t: &Terminal) -> String {
        let grid = t.term.grid();
        let mut out = String::new();
        for row in 0..t.rows as i32 {
            let grid_row = &grid[Line(row - t.display_offset() as i32)];
            for col in 0..t.cols as usize {
                out.push(grid_row[Column(col)].c);
            }
            out.push(chr_nl());
        }
        out
    }
    fn chr_nl() -> char { '\n' }

    #[test]
    fn cursor_tracks_resize() {
        let (input_tx, _rx) = mpsc::unbounded_channel();
        let (mut t, _ti, _be) = Terminal::new(80, 24, 10000, input_tx);
        t.feed(b"prompt> ");
        let p1 = t.term.grid().cursor.point;
        assert_eq!((p1.line.0, p1.column.0), (0, 8), "before resize");
        // font size grows: viewport shrinks
        t.resize(40, 10);
        let p2 = t.term.grid().cursor.point;
        println!("after resize: line={} col={}, offset={}", p2.line.0, p2.column.0, t.display_offset());
        assert_eq!((p2.line.0, p2.column.0), (0, 8), "cursor must keep its cell after resize");
        // grow viewport back
        t.resize(80, 24);
        let p3 = t.term.grid().cursor.point;
        assert_eq!((p3.line.0, p3.column.0), (0, 8), "cursor must keep its cell after grow");
        let _ = dump(&t);
    }
}
