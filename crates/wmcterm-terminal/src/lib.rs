use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use libghostty_vt::key::{Action as KeyAction, Encoder, Event as KeyEvent, Key, Mods};
use libghostty_vt::mouse::{
    Action as GhosttyMouseAction, Button as GhosttyMouseButton, Encoder as MouseEncoder,
    EncoderSize as MouseEncoderSize, Event as GhosttyMouseEvent, Position as MousePosition,
};
use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::screen::CellWide;
use libghostty_vt::selection::{FormatOptions, Selection};
use libghostty_vt::style::{Palette, PaletteIndex, RgbColor, Underline};
use libghostty_vt::terminal::{ColorScheme, Mode, Point, PointCoordinate, ScrollViewport};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use thiserror::Error;
use wmcterm_agent::{AgentEvent, OscEvent, OscScanner};
use wmcterm_pty::{CommandSpec, PtyCommand, PtyEvent, PtySession, PtySize};

mod ssh;

/// Ghostty measures scrollback capacity in bytes, not rows. Keep the standalone
/// terminal constructors aligned with Ghostty's default allocation.
pub const DEFAULT_SCROLLBACK_BYTES: usize = 128_000_000;
pub const DEFAULT_SCROLLBACK_LINES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrollbackLimits {
    bytes: usize,
    rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub fn new(cols: u16, rows: u16) -> Result<Self, TerminalError> {
        if cols == 0 || rows == 0 {
            return Err(TerminalError::InvalidSize { cols, rows });
        }
        Ok(Self { cols, rows })
    }
}

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("terminal dimensions must be non-zero, got {cols}x{rows}")]
    InvalidSize { cols: u16, rows: u16 },
    #[error("libghostty-vt error: {0}")]
    Ghostty(#[from] libghostty_vt::Error),
    #[error("terminal runtime error: {0}")]
    Runtime(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub lines: Vec<String>,
    pub cells: Vec<Vec<CellSnapshot>>,
    pub cursor: Option<CursorSnapshot>,
    pub mouse_tracking: bool,
    pub keyboard_flags: u8,
    pub focus_reporting: bool,
    pub bracketed_paste: bool,
    pub scrollback_rows: usize,
    pub viewport_offset: usize,
    /// Absolute first row represented by `cells` in libghostty's full screen.
    pub screen_row_origin: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionPoint {
    pub column: u16,
    /// Absolute row in the full screen, including scrollback.
    pub screen_row: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseActionInput {
    Press,
    Release,
    Motion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButtonInput {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseInput {
    pub action: MouseActionInput,
    pub button: Option<MouseButtonInput>,
    pub x: f32,
    pub y: f32,
    pub cell_width: u32,
    pub cell_height: u32,
    pub modifiers: KeyModifiers,
    pub any_button_pressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSnapshot {
    pub text: String,
    /// OSC 8 hyperlink attached to this cell, when present.
    pub hyperlink: Option<Arc<str>>,
    pub foreground: Rgb,
    pub background: Rgb,
    /// The cell requests a non-default background, including SGR inverse.
    pub background_explicit: bool,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalTheme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub palette: Vec<Rgb>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorSnapshot {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    pub blinking: bool,
}

impl ScreenSnapshot {
    pub fn to_plain_text(&self) -> String {
        let mut lines = self.lines.clone();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }
}

pub struct TerminalEngine {
    terminal: Terminal<'static, 'static>,
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    key_encoder: Encoder<'static>,
    size: TerminalSize,
    effects: Arc<Mutex<Vec<TerminalEffect>>>,
    color_scheme: Arc<Mutex<ColorScheme>>,
    search_query: String,
    search_index: Option<usize>,
    viewport_offset: usize,
    max_scrollback_rows: usize,
}

impl TerminalEngine {
    pub fn new(size: TerminalSize) -> Result<Self, TerminalError> {
        Self::new_with_scrollback(size, DEFAULT_SCROLLBACK_BYTES)
    }

    pub fn new_with_scrollback(
        size: TerminalSize,
        max_scrollback: usize,
    ) -> Result<Self, TerminalError> {
        Self::new_with_scrollback_limits(size, max_scrollback, DEFAULT_SCROLLBACK_LINES)
    }

    pub fn new_with_scrollback_limits(
        size: TerminalSize,
        max_scrollback: usize,
        max_scrollback_rows: usize,
    ) -> Result<Self, TerminalError> {
        let effects = Arc::new(Mutex::new(Vec::new()));
        let color_scheme = Arc::new(Mutex::new(ColorScheme::Dark));
        let mut terminal = Terminal::new(TerminalOptions {
            cols: size.cols,
            rows: size.rows,
            max_scrollback,
        })?;
        let queue = Arc::clone(&effects);
        terminal.on_pty_write(move |_terminal, bytes| {
            queue
                .lock()
                .expect("effect queue poisoned")
                .push(TerminalEffect::PtyWrite(bytes.to_vec()))
        })?;
        let queue = Arc::clone(&effects);
        terminal.on_bell(move |_terminal| {
            queue
                .lock()
                .expect("effect queue poisoned")
                .push(TerminalEffect::Bell)
        })?;
        let queue = Arc::clone(&effects);
        terminal.on_title_changed(move |terminal| {
            if let Ok(title) = terminal.title() {
                queue
                    .lock()
                    .expect("effect queue poisoned")
                    .push(TerminalEffect::TitleChanged(title.to_owned()));
            }
        })?;
        let queue = Arc::clone(&effects);
        terminal.on_pwd_changed(move |terminal| {
            if let Ok(cwd) = terminal.pwd() {
                queue
                    .lock()
                    .expect("effect queue poisoned")
                    .push(TerminalEffect::CwdChanged(cwd.to_owned()));
            }
        })?;
        let reported_scheme = Arc::clone(&color_scheme);
        terminal.on_color_scheme(move |_terminal| {
            Some(*reported_scheme.lock().expect("color scheme lock poisoned"))
        })?;
        Ok(Self {
            terminal,
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            key_encoder: Encoder::new()?,
            size,
            effects,
            color_scheme,
            search_query: String::new(),
            search_index: None,
            viewport_offset: 0,
            max_scrollback_rows,
        })
    }

    pub fn apply_theme(&mut self, theme: &TerminalTheme) -> Result<(), TerminalError> {
        let foreground = ghostty_rgb(theme.foreground);
        let background = ghostty_rgb(theme.background);
        *self
            .color_scheme
            .lock()
            .expect("color scheme lock poisoned") = if background.perceived_luminance() > 0.5 {
            ColorScheme::Light
        } else {
            ColorScheme::Dark
        };
        self.terminal
            .set_default_fg_color(Some(foreground))?
            .set_default_bg_color(Some(background))?
            .set_default_cursor_color(Some(ghostty_rgb(theme.cursor)))?;
        let mut base = Palette::default();
        for (index, color) in theme.palette.iter().take(16).copied().enumerate() {
            base.set(PaletteIndex(index as u8), ghostty_rgb(color));
        }
        let palette = Palette::generate(Some(&base), None, background, foreground, true);
        self.terminal.set_default_color_palette(Some(palette))?;
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let previous_scrollback_rows = self.terminal.scrollback_rows().unwrap_or(0);
        self.terminal.vt_write(bytes);
        if self.viewport_offset > 0 {
            let scrollback_rows = self.terminal.scrollback_rows().unwrap_or(0);
            let maximum = scrollback_rows.min(self.max_scrollback_rows);
            self.viewport_offset = self
                .viewport_offset
                .saturating_add(scrollback_rows.saturating_sub(previous_scrollback_rows))
                .min(maximum);
        }
    }

    pub fn resize(
        &mut self,
        size: TerminalSize,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), TerminalError> {
        self.terminal
            .resize(size.cols, size.rows, cell_width_px, cell_height_px)?;
        self.size = size;
        Ok(())
    }

    /// Whether the application is inside a DEC 2026 synchronized-output
    /// transaction. A host should not present the partially updated grid.
    pub fn synchronized_output(&self) -> bool {
        self.terminal.mode(Mode::SYNC_OUTPUT).unwrap_or(false)
    }

    pub fn snapshot(&mut self) -> Result<ScreenSnapshot, TerminalError> {
        let snapshot = self.render_state.update(&self.terminal)?;
        let cols = snapshot.cols()?;
        let rows = snapshot.rows()?;
        let colors = snapshot.colors()?;
        let default_fg = Rgb {
            r: colors.foreground.r,
            g: colors.foreground.g,
            b: colors.foreground.b,
        };
        let default_bg = Rgb {
            r: colors.background.r,
            g: colors.background.g,
            b: colors.background.b,
        };
        let cursor = snapshot.cursor_viewport()?.map(|cursor| CursorSnapshot {
            x: cursor.x,
            y: cursor.y,
            visible: snapshot.cursor_visible().unwrap_or(false),
            blinking: snapshot.cursor_blinking().unwrap_or(false),
        });
        let mut lines = Vec::with_capacity(rows as usize);
        let mut all_cells = Vec::with_capacity(rows as usize);
        let mut row_iter = self.rows.update(&snapshot)?;

        let mut viewport_y = 0_u32;
        let mut hyperlink_buffer = Vec::new();
        while let Some(row) = row_iter.next() {
            let mut line = String::new();
            let mut row_cells = Vec::with_capacity(cols as usize);
            let mut previous_hyperlink: Option<Arc<str>> = None;
            let mut cell_iter = self.cells.update(row)?;
            let mut viewport_x = 0_u16;
            while let Some(cell) = cell_iter.next() {
                let graphemes = cell.graphemes()?;
                let raw_cell = cell.raw_cell()?;
                let wide = raw_cell.wide()?;
                let hyperlink = if !raw_cell.has_hyperlink()? {
                    None
                } else {
                    let grid_ref = self.terminal.grid_ref(Point::Viewport(PointCoordinate {
                        x: viewport_x,
                        y: viewport_y,
                    }))?;
                    match grid_ref.hyperlink_uri(&mut []) {
                        Ok(0) => None,
                        Ok(_) => None,
                        Err(libghostty_vt::Error::OutOfSpace { required })
                            if required <= 16_384 =>
                        {
                            hyperlink_buffer.resize(required, 0);
                            let length = grid_ref.hyperlink_uri(&mut hyperlink_buffer)?;
                            match std::str::from_utf8(&hyperlink_buffer[..length]) {
                                Ok(uri) if previous_hyperlink.as_deref() == Some(uri) => {
                                    previous_hyperlink.clone()
                                }
                                Ok(uri) => {
                                    let uri: Arc<str> = Arc::from(uri);
                                    previous_hyperlink = Some(uri.clone());
                                    Some(uri)
                                }
                                Err(_) => None,
                            }
                        }
                        Err(libghostty_vt::Error::OutOfSpace { .. }) => None,
                        Err(error) => return Err(error.into()),
                    }
                };
                let text: String = if graphemes.is_empty()
                    && matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead)
                {
                    String::new()
                } else if graphemes.is_empty() {
                    " ".into()
                } else {
                    graphemes.iter().collect()
                };
                if graphemes.is_empty()
                    && !matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead)
                {
                    line.push(' ');
                } else if !graphemes.is_empty() {
                    line.extend(graphemes);
                }
                let style = cell.style()?;
                let foreground_color = cell.fg_color()?;
                let background_color = cell.bg_color()?;
                let mut foreground = foreground_color
                    .map(|color| Rgb {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                    })
                    .unwrap_or(default_fg);
                let mut background = background_color
                    .map(|color| Rgb {
                        r: color.r,
                        g: color.g,
                        b: color.b,
                    })
                    .unwrap_or(default_bg);
                if style.inverse {
                    std::mem::swap(&mut foreground, &mut background);
                }
                row_cells.push(CellSnapshot {
                    text,
                    hyperlink,
                    foreground,
                    background,
                    background_explicit: background_color.is_some() || style.inverse,
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                    strikethrough: style.strikethrough,
                });
                viewport_x = viewport_x.saturating_add(1);
            }
            lines.push(line.trim_end().to_owned());
            all_cells.push(row_cells);
            viewport_y = viewport_y.saturating_add(1);
        }
        let actual_scrollback_rows = self.terminal.scrollback_rows()?;
        Ok(ScreenSnapshot {
            cols,
            rows,
            lines,
            cells: all_cells,
            cursor,
            mouse_tracking: self.terminal.is_mouse_tracking()?,
            keyboard_flags: self.terminal.kitty_keyboard_flags()?.bits(),
            focus_reporting: self.terminal.mode(Mode::FOCUS_EVENT)?,
            bracketed_paste: self.terminal.mode(Mode::BRACKETED_PASTE)?,
            scrollback_rows: actual_scrollback_rows.min(self.max_scrollback_rows),
            viewport_offset: self.viewport_offset,
            screen_row_origin: actual_scrollback_rows.saturating_sub(self.viewport_offset),
        })
    }

    pub fn encode_mouse(&mut self, input: MouseInput) -> Result<Vec<u8>, TerminalError> {
        let mut encoder = MouseEncoder::new()?;
        encoder
            .set_options_from_terminal(&self.terminal)
            .set_size(MouseEncoderSize {
                screen_width: u32::from(self.size.cols) * input.cell_width,
                screen_height: u32::from(self.size.rows) * input.cell_height,
                cell_width: input.cell_width,
                cell_height: input.cell_height,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_any_button_pressed(input.any_button_pressed)
            .set_track_last_cell(true);
        let mut event = GhosttyMouseEvent::new()?;
        event
            .set_action(match input.action {
                MouseActionInput::Press => GhosttyMouseAction::Press,
                MouseActionInput::Release => GhosttyMouseAction::Release,
                MouseActionInput::Motion => GhosttyMouseAction::Motion,
            })
            .set_button(input.button.map(|button| match button {
                MouseButtonInput::Left => GhosttyMouseButton::Left,
                MouseButtonInput::Middle => GhosttyMouseButton::Middle,
                MouseButtonInput::Right => GhosttyMouseButton::Right,
                MouseButtonInput::WheelUp => GhosttyMouseButton::Four,
                MouseButtonInput::WheelDown => GhosttyMouseButton::Five,
            }))
            .set_mods(input.modifiers.into())
            .set_position(MousePosition {
                x: input.x,
                y: input.y,
            });
        let mut bytes = Vec::with_capacity(32);
        encoder.encode_to_vec(&event, &mut bytes)?;
        Ok(bytes)
    }

    pub fn clear_scrollback(&mut self) {
        self.feed(b"\x1b[2J\x1b[3J\x1b[H");
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.viewport_offset = 0;
    }

    pub fn search(&mut self, query: &str, direction: isize) -> Result<SearchResult, TerminalError> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            self.search_query.clear();
            self.search_index = None;
            return Ok(SearchResult {
                query,
                current: 0,
                total: 0,
            });
        }
        let mut matches = Vec::new();
        let total_rows = self.terminal.total_rows()?;
        let first_row = total_rows.saturating_sub(
            self.max_scrollback_rows
                .saturating_add(self.size.rows as usize),
        );
        for row in first_row..total_rows {
            let mut line = String::new();
            for col in 0..self.size.cols {
                let reference = self.terminal.grid_ref(Point::Screen(PointCoordinate {
                    x: col,
                    y: row as u32,
                }))?;
                let mut graphemes = ['\0'; 16];
                let len = reference.graphemes(&mut graphemes).unwrap_or(0);
                if len == 0 {
                    line.push(' ');
                } else {
                    line.extend(graphemes[..len].iter());
                }
            }
            if line.to_lowercase().contains(&query) {
                matches.push(row);
            }
        }
        let reset = self.search_query != query;
        self.search_query = query.clone();
        self.search_index = if matches.is_empty() {
            None
        } else if reset {
            Some(if direction < 0 { matches.len() - 1 } else { 0 })
        } else {
            Some(match (self.search_index.unwrap_or(0), direction < 0) {
                (0, true) => matches.len() - 1,
                (index, true) => index - 1,
                (index, false) => (index + 1) % matches.len(),
            })
        };
        if let Some(index) = self.search_index {
            let viewport_row = matches[index]
                .saturating_sub(self.size.rows as usize / 2)
                .max(first_row);
            self.terminal
                .scroll_viewport(ScrollViewport::Row(viewport_row));
            self.viewport_offset = self
                .terminal
                .scrollback_rows()?
                .saturating_sub(viewport_row);
        }
        Ok(SearchResult {
            query,
            current: self.search_index.map_or(0, |index| index + 1),
            total: matches.len(),
        })
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    pub fn take_effects(&self) -> Vec<TerminalEffect> {
        std::mem::take(&mut *self.effects.lock().expect("effect queue poisoned"))
    }

    pub fn scroll(&mut self, lines: isize) {
        let maximum = self
            .terminal
            .scrollback_rows()
            .unwrap_or(0)
            .min(self.max_scrollback_rows);
        let applied = if lines < 0 {
            let amount = lines
                .unsigned_abs()
                .min(maximum.saturating_sub(self.viewport_offset));
            self.viewport_offset += amount;
            -(amount as isize)
        } else {
            let amount = (lines as usize).min(self.viewport_offset);
            self.viewport_offset -= amount;
            amount as isize
        };
        if applied != 0 {
            self.terminal
                .scroll_viewport(ScrollViewport::Delta(applied));
        }
    }

    /// Return a manually scrolled viewport to the live terminal screen.
    /// Returns whether the visible viewport changed.
    pub fn scroll_to_bottom(&mut self) -> bool {
        if self.viewport_offset == 0 {
            return false;
        }
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.viewport_offset = 0;
        true
    }

    pub fn select_all_text(&self) -> Result<String, TerminalError> {
        let total_rows = self.terminal.total_rows()?;
        if total_rows == 0 {
            return Ok(String::new());
        }
        let first_row = total_rows.saturating_sub(
            self.max_scrollback_rows
                .saturating_add(self.size.rows as usize),
        );
        let start = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: 0,
            y: first_row as u32,
        }))?;
        let end = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: self.size.cols.saturating_sub(1),
            y: total_rows.saturating_sub(1) as u32,
        }))?;
        let selection = Selection::new(start, end, false);
        let options = FormatOptions::new()
            .with_selection(&selection)
            .with_unwrap(true)
            .with_trim(true);
        let Some(bytes) = self.terminal.format_selection_alloc(None, options)? else {
            return Ok(String::new());
        };
        Ok(String::from_utf8_lossy(bytes.as_ref()).into_owned())
    }

    pub fn selection_text(
        &self,
        start: SelectionPoint,
        end: SelectionPoint,
    ) -> Result<String, TerminalError> {
        let total_rows = self.terminal.total_rows()?;
        if total_rows == 0 {
            return Ok(String::new());
        }
        let max_column = self.size.cols.saturating_sub(1);
        let max_row = total_rows.saturating_sub(1).min(u32::MAX as usize) as u32;
        let start = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: start.column.min(max_column),
            y: start.screen_row.min(max_row),
        }))?;
        let end = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: end.column.min(max_column),
            y: end.screen_row.min(max_row),
        }))?;
        let selection = Selection::new(start, end, false);
        let options = FormatOptions::new()
            .with_selection(&selection)
            .with_unwrap(true)
            .with_trim(true);
        let Some(bytes) = self.terminal.format_selection_alloc(None, options)? else {
            return Ok(String::new());
        };
        Ok(String::from_utf8_lossy(bytes.as_ref()).into_owned())
    }

    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>, TerminalError> {
        // The encoder maps CR and LF individually; fold Windows line endings
        // first so each clipboard line produces exactly one terminal newline.
        let mut input = text.replace("\r\n", "\n").into_bytes();
        let mut output = vec![0_u8; input.len() + 16];
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let written = libghostty_vt::paste::encode(&mut input, bracketed, &mut output)?;
        output.truncate(written);
        Ok(output)
    }

    pub fn encode_key(&mut self, input: &KeyInput) -> Result<Vec<u8>, TerminalError> {
        self.encode_key_action(
            input,
            if input.repeat {
                KeyAction::Repeat
            } else {
                KeyAction::Press
            },
        )
    }

    pub fn encode_key_release(&mut self, input: &KeyInput) -> Result<Vec<u8>, TerminalError> {
        self.encode_key_action(input, KeyAction::Release)
    }

    pub fn encode_focus(&self, focused: bool) -> Result<Vec<u8>, TerminalError> {
        if !self.terminal.mode(Mode::FOCUS_EVENT)? {
            return Ok(Vec::new());
        }
        let event = if focused {
            libghostty_vt::focus::Event::Gained
        } else {
            libghostty_vt::focus::Event::Lost
        };
        let mut bytes = [0; 8];
        let len = event.encode(&mut bytes)?;
        Ok(bytes[..len].to_vec())
    }

    fn encode_key_action(
        &mut self,
        input: &KeyInput,
        action: KeyAction,
    ) -> Result<Vec<u8>, TerminalError> {
        // Preserve legacy control bytes only without keyboard negotiation.
        // Platform printable text must not override Control in either protocol.
        if self.terminal.kitty_keyboard_flags()?.is_empty()
            && action != KeyAction::Release
            && input.modifiers.control
            && !input.modifiers.shift
            && !input.modifiers.alt
            && !input.modifiers.super_key
            && let Some(byte) = conventional_control_byte(input.key)
        {
            return Ok(vec![byte]);
        }
        let mut event = KeyEvent::new()?;
        event
            .set_action(action)
            .set_key(input.key.into())
            .set_mods(input.modifiers.into());
        if let Some(character) = input.unshifted_codepoint {
            event.set_unshifted_codepoint(character);
        } else {
            match input.key {
                InputKey::Character(character) => {
                    event.set_unshifted_codepoint(character.to_ascii_lowercase());
                }
                InputKey::Space => {
                    event.set_unshifted_codepoint(' ');
                }
                _ => {}
            }
        }
        if let Some(text) = input.text.as_ref().filter(|text| {
            valid_key_text(text) && !input.modifiers.control && action != KeyAction::Release
        }) {
            event.set_utf8(Some(text.clone()));
        }
        self.key_encoder.set_options_from_terminal(&self.terminal);
        let mut bytes = Vec::with_capacity(16);
        self.key_encoder.encode_to_vec(&event, &mut bytes)?;
        Ok(bytes)
    }
}

fn conventional_control_byte(key: InputKey) -> Option<u8> {
    match key {
        InputKey::Space | InputKey::Character('@' | '`') => Some(0x00),
        InputKey::Character(character @ ('a'..='z' | 'A'..='Z')) => {
            Some(character.to_ascii_uppercase() as u8 - b'@')
        }
        InputKey::Character('[' | '{') => Some(0x1b),
        InputKey::Character('\\' | '|') => Some(0x1c),
        InputKey::Character(']' | '}') => Some(0x1d),
        InputKey::Character('^' | '~') => Some(0x1e),
        InputKey::Character('_' | '/') => Some(0x1f),
        InputKey::Character('?') => Some(0x7f),
        _ => None,
    }
}

fn valid_key_text(text: &str) -> bool {
    !text.chars().any(|character| {
        matches!(character, '\0'..='\u{1f}' | '\u{7f}')
            || ('\u{f700}'..='\u{f8ff}').contains(&character)
    })
}

const fn ghostty_rgb(color: Rgb) -> RgbColor {
    RgbColor {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKey {
    Function(u8),
    Character(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    Space,
}

impl From<InputKey> for Key {
    fn from(value: InputKey) -> Self {
        match value {
            InputKey::Function(number) => [
                Key::F1,
                Key::F2,
                Key::F3,
                Key::F4,
                Key::F5,
                Key::F6,
                Key::F7,
                Key::F8,
                Key::F9,
                Key::F10,
                Key::F11,
                Key::F12,
                Key::F13,
                Key::F14,
                Key::F15,
                Key::F16,
                Key::F17,
                Key::F18,
                Key::F19,
                Key::F20,
                Key::F21,
                Key::F22,
                Key::F23,
                Key::F24,
            ]
            .get(number.wrapping_sub(1) as usize)
            .copied()
            .unwrap_or(Key::Unidentified),
            InputKey::Character(c) => match c.to_ascii_lowercase() {
                'a' => Key::A,
                'b' => Key::B,
                'c' => Key::C,
                'd' => Key::D,
                'e' => Key::E,
                'f' => Key::F,
                'g' => Key::G,
                'h' => Key::H,
                'i' => Key::I,
                'j' => Key::J,
                'k' => Key::K,
                'l' => Key::L,
                'm' => Key::M,
                'n' => Key::N,
                'o' => Key::O,
                'p' => Key::P,
                'q' => Key::Q,
                'r' => Key::R,
                's' => Key::S,
                't' => Key::T,
                'u' => Key::U,
                'v' => Key::V,
                'w' => Key::W,
                'x' => Key::X,
                'y' => Key::Y,
                'z' => Key::Z,
                _ => Key::Unidentified,
            },
            InputKey::Enter => Key::Enter,
            InputKey::Backspace => Key::Backspace,
            InputKey::Tab => Key::Tab,
            InputKey::Escape => Key::Escape,
            InputKey::Up => Key::ArrowUp,
            InputKey::Down => Key::ArrowDown,
            InputKey::Left => Key::ArrowLeft,
            InputKey::Right => Key::ArrowRight,
            InputKey::Home => Key::Home,
            InputKey::End => Key::End,
            InputKey::PageUp => Key::PageUp,
            InputKey::PageDown => Key::PageDown,
            InputKey::Delete => Key::Delete,
            InputKey::Insert => Key::Insert,
            InputKey::Space => Key::Space,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
    pub super_key: bool,
}

impl From<KeyModifiers> for Mods {
    fn from(value: KeyModifiers) -> Self {
        let mut mods = Mods::empty();
        if value.shift {
            mods |= Mods::SHIFT;
        }
        if value.alt {
            mods |= Mods::ALT;
        }
        if value.control {
            mods |= Mods::CTRL;
        }
        if value.super_key {
            mods |= Mods::SUPER;
        }
        mods
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInput {
    pub key: InputKey,
    pub unshifted_codepoint: Option<char>,
    pub text: Option<String>,
    pub modifiers: KeyModifiers,
    pub repeat: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEffect {
    PtyWrite(Vec<u8>),
    Bell,
    TitleChanged(String),
    CwdChanged(String),
}

#[derive(Debug, Clone)]
pub enum SessionCommand {
    Ssh {
        method: String,
        params: serde_json::Value,
        deadline: std::time::Instant,
        reply: std::sync::mpsc::SyncSender<Result<serde_json::Value, String>>,
    },
    Input(Vec<u8>),
    Key(KeyInput),
    KeyRelease(KeyInput),
    Focus(bool),
    Paste(String),
    PasteLine(String),
    CopyAll,
    CopySelection {
        start: SelectionPoint,
        end: SelectionPoint,
        target: ClipboardTarget,
    },
    Mouse(MouseInput),
    Resize {
        size: TerminalSize,
        cell_width_px: u32,
        cell_height_px: u32,
    },
    Scroll(isize),
    Search {
        query: String,
        direction: isize,
    },
    ClearScrollback,
    ExportHistory(std::sync::mpsc::SyncSender<String>),
    RestoreHistory(String),
    SetTheme(TerminalTheme),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Snapshot(ScreenSnapshot),
    TitleChanged(String),
    Bell,
    CwdChanged(String),
    ForegroundProcess(Vec<String>),
    ChildProcesses(Vec<String>),
    Agent(AgentEvent),
    ProgramStatus(AgentEvent),
    Notification(String),
    ClipboardText {
        text: String,
        target: ClipboardTarget,
    },
    SearchResult(SearchResult),
    Exited(String),
    Warning(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub query: String,
    pub current: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardTarget {
    Clipboard,
    Primary,
}

pub struct TerminalSession {
    commands: flume::Sender<SessionCommand>,
    events: flume::Receiver<SessionEvent>,
    worker: Option<JoinHandle<()>>,
    final_ssh_status: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

impl TerminalSession {
    pub fn spawn(command: CommandSpec, size: TerminalSize) -> Result<Self, TerminalError> {
        Self::spawn_with_scrollback(command, size, DEFAULT_SCROLLBACK_BYTES)
    }

    pub fn spawn_with_scrollback(
        command: CommandSpec,
        size: TerminalSize,
        max_scrollback: usize,
    ) -> Result<Self, TerminalError> {
        Self::spawn_with_scrollback_and_history(command, size, max_scrollback, None)
    }

    pub fn spawn_with_scrollback_and_history(
        command: CommandSpec,
        size: TerminalSize,
        max_scrollback: usize,
        history: Option<String>,
    ) -> Result<Self, TerminalError> {
        Self::spawn_with_scrollback_history_and_theme(command, size, max_scrollback, history, None)
    }

    pub fn spawn_with_scrollback_history_and_theme(
        command: CommandSpec,
        size: TerminalSize,
        max_scrollback: usize,
        history: Option<String>,
        theme: Option<TerminalTheme>,
    ) -> Result<Self, TerminalError> {
        Self::spawn_with_scrollback_limits_history_and_theme(
            command,
            size,
            max_scrollback,
            DEFAULT_SCROLLBACK_LINES,
            history,
            theme,
        )
    }

    pub fn spawn_with_scrollback_limits_history_and_theme(
        command: CommandSpec,
        size: TerminalSize,
        max_scrollback: usize,
        max_scrollback_rows: usize,
        history: Option<String>,
        theme: Option<TerminalTheme>,
    ) -> Result<Self, TerminalError> {
        let ssh = ssh::SshState::default();
        let final_ssh_status = ssh.final_status.clone();
        let command = command.with_bash_rc(&ssh.local_rc());
        let pty = PtySession::spawn(command, PtySize::new(size.cols, size.rows))
            .map_err(|error| TerminalError::Runtime(error.to_string()))?;
        let (command_tx, command_rx) = flume::unbounded();
        let (event_tx, event_rx) = flume::unbounded();
        let worker = thread::Builder::new()
            .name("wmcterm-terminal".into())
            .spawn(move || {
                run_terminal_actor(
                    pty,
                    size,
                    ScrollbackLimits {
                        bytes: max_scrollback,
                        rows: max_scrollback_rows,
                    },
                    history,
                    theme,
                    command_rx,
                    event_tx,
                    ssh,
                )
            })
            .map_err(|error| TerminalError::Runtime(error.to_string()))?;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            worker: Some(worker),
            final_ssh_status,
        })
    }
    pub fn send(&self, command: SessionCommand) -> Result<(), flume::SendError<SessionCommand>> {
        self.commands.send(command)
    }
    pub fn events(&self) -> &flume::Receiver<SessionEvent> {
        &self.events
    }

    pub fn export_history(&self) -> Option<String> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        self.commands
            .send(SessionCommand::ExportHistory(sender))
            .ok()?;
        receiver
            .recv_timeout(std::time::Duration::from_millis(500))
            .ok()
    }

    pub fn ssh_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if method == "ssh.status"
            && let Some(status) = self
                .final_ssh_status
                .lock()
                .map_err(|e| e.to_string())?
                .clone()
        {
            return Ok(status);
        }
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        self.send(SessionCommand::Ssh {
            method: method.into(),
            params,
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(400),
            reply,
        })
        .map_err(|e| e.to_string())?;
        receiver
            .recv_timeout(std::time::Duration::from_millis(500))
            .map_err(|_| {
                "SSH actor response unavailable; query status before retrying".to_owned()
            })?
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.commands.send(SessionCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const PTY_OUTPUT_SNAPSHOT_INTERVAL: std::time::Duration =
    std::time::Duration::from_nanos(16_666_667);
const MAX_PROTOCOL_REPLY_BYTES_PER_BURST: usize = 1024;
const PROTOCOL_REPLY_BURST_QUIET: std::time::Duration = std::time::Duration::from_millis(250);

struct ProtocolReplyLimiter {
    remaining: usize,
    last_output: Option<std::time::Instant>,
}

#[derive(Debug, Default)]
struct OutputSnapshotSchedule {
    deadline: Option<std::time::Instant>,
}

impl OutputSnapshotSchedule {
    fn mark_dirty(&mut self, now: std::time::Instant) {
        // Keep the deadline anchored to the first unpresented output. Moving it
        // on every chunk would allow a busy PTY to postpone rendering forever.
        self.deadline
            .get_or_insert(now + PTY_OUTPUT_SNAPSHOT_INTERVAL);
    }

    fn deadline(&self) -> Option<std::time::Instant> {
        self.deadline
    }

    fn is_due(&self, now: std::time::Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    fn is_dirty(&self) -> bool {
        self.deadline.is_some()
    }

    fn mark_presented(&mut self) {
        self.deadline = None;
    }
}

fn publish_snapshot(
    engine: &mut TerminalEngine,
    events: &flume::Sender<SessionEvent>,
    schedule: &mut OutputSnapshotSchedule,
    force: bool,
) {
    if engine.synchronized_output() && !force {
        schedule.mark_dirty(std::time::Instant::now());
        return;
    }
    match engine.snapshot() {
        Ok(snapshot) => {
            let _ = events.send(SessionEvent::Snapshot(snapshot));
        }
        Err(error) => {
            let _ = events.send(SessionEvent::Warning(format!("snapshot: {error}")));
        }
    }
    schedule.mark_presented();
}

impl ProtocolReplyLimiter {
    fn new() -> Self {
        Self {
            remaining: MAX_PROTOCOL_REPLY_BYTES_PER_BURST,
            last_output: None,
        }
    }

    fn begin_output_chunk(&mut self, now: std::time::Instant) {
        if self
            .last_output
            .is_none_or(|last| now.saturating_duration_since(last) >= PROTOCOL_REPLY_BURST_QUIET)
        {
            self.remaining = MAX_PROTOCOL_REPLY_BYTES_PER_BURST;
        }
        self.last_output = Some(now);
    }

    fn accept(&mut self, bytes: usize) -> bool {
        if bytes > self.remaining {
            self.remaining = 0;
            return false;
        }
        self.remaining -= bytes;
        true
    }
}

struct DrainedTerminalEffects {
    protocol_reply: Vec<u8>,
    remaining: Vec<TerminalEffect>,
    reply_truncated: bool,
}

fn drain_terminal_effects(
    engine: &TerminalEngine,
    reply_limiter: &mut ProtocolReplyLimiter,
) -> DrainedTerminalEffects {
    let mut protocol_reply = Vec::new();
    let mut remaining = Vec::new();
    let mut reply_truncated = false;
    for effect in engine.take_effects() {
        match effect {
            TerminalEffect::PtyWrite(bytes) if reply_limiter.accept(bytes.len()) => {
                protocol_reply.extend(bytes);
            }
            TerminalEffect::PtyWrite(_) => reply_truncated = true,
            effect => remaining.push(effect),
        }
    }
    DrainedTerminalEffects {
        protocol_reply,
        remaining,
        reply_truncated,
    }
}

fn forward_terminal_effects(
    engine: &TerminalEngine,
    pty: &PtySession,
    events: &flume::Sender<SessionEvent>,
    reply_limiter: &mut ProtocolReplyLimiter,
    reply_limit_warned: &mut bool,
) {
    let drained = drain_terminal_effects(engine, reply_limiter);
    let reply_backlogged = if drained.protocol_reply.is_empty() {
        false
    } else {
        !pty.send_protocol_reply(drained.protocol_reply)
            .unwrap_or(false)
    };
    if (drained.reply_truncated || reply_backlogged) && !*reply_limit_warned {
        tracing::warn!("dropping excessive terminal protocol replies generated by PTY output");
        *reply_limit_warned = true;
    }
    for effect in drained.remaining {
        match effect {
            TerminalEffect::PtyWrite(_) => unreachable!("PTY writes were drained above"),
            TerminalEffect::Bell => {
                let _ = events.send(SessionEvent::Bell);
            }
            TerminalEffect::TitleChanged(title) => {
                let _ = events.send(SessionEvent::TitleChanged(title));
            }
            TerminalEffect::CwdChanged(cwd) => {
                let _ = events.send(SessionEvent::CwdChanged(cwd));
            }
        }
    }
}

fn process_pty_output_chunk(
    bytes: &[u8],
    engine: &mut TerminalEngine,
    osc: &mut OscScanner,
    pty: &PtySession,
    events: &flume::Sender<SessionEvent>,
    reply_limiter: &mut ProtocolReplyLimiter,
    reply_limit_warned: &mut bool,
) {
    reply_limiter.begin_output_chunk(std::time::Instant::now());
    for event in osc.feed(bytes) {
        match event {
            OscEvent::ProgramStatus(state) => {
                let _ = events.send(SessionEvent::ProgramStatus(AgentEvent::StateObserved {
                    state,
                }));
            }
            OscEvent::Agent(event) => {
                let _ = events.send(SessionEvent::Agent(event));
            }
            OscEvent::Notification(message) => {
                let _ = events.send(SessionEvent::Notification(message));
            }
            OscEvent::PtyWrite(bytes) => {
                let accepted = reply_limiter.accept(bytes.len())
                    && pty.send_protocol_reply(bytes).unwrap_or(false);
                if !accepted && !*reply_limit_warned {
                    tracing::warn!(
                        "dropping excessive terminal protocol replies generated by OSC output"
                    );
                    *reply_limit_warned = true;
                }
            }
        }
    }
    engine.feed(bytes);
    forward_terminal_effects(engine, pty, events, reply_limiter, reply_limit_warned);
}

#[allow(clippy::too_many_arguments)]
fn run_terminal_actor(
    pty: PtySession,
    size: TerminalSize,
    scrollback: ScrollbackLimits,
    history: Option<String>,
    theme: Option<TerminalTheme>,
    commands: flume::Receiver<SessionCommand>,
    events: flume::Sender<SessionEvent>,
    mut ssh: ssh::SshState,
) {
    let Ok(mut engine) =
        TerminalEngine::new_with_scrollback_limits(size, scrollback.bytes, scrollback.rows)
    else {
        let _ = events.send(SessionEvent::Error("terminal initialization failed".into()));
        return;
    };
    if let Some(theme) = theme
        && let Err(error) = engine.apply_theme(&theme)
    {
        let _ = events.send(SessionEvent::Error(format!(
            "terminal theme initialization: {error}"
        )));
        return;
    }
    if let Some(history) = history.filter(|history| !history.is_empty()) {
        engine.feed(&normalize_restored_history(&history));
        // Persisted output is render-only replay. Never send terminal replies,
        // bells, or title changes back into a newly spawned PTY.
        let _ = engine.take_effects();
        match engine.snapshot() {
            Ok(snapshot) => {
                let _ = events.send(SessionEvent::Snapshot(snapshot));
            }
            Err(error) => {
                let _ = events.send(SessionEvent::Warning(format!(
                    "restored history snapshot: {error}"
                )));
            }
        }
    }
    let mut osc = OscScanner::default();
    let mut reply_limiter = ProtocolReplyLimiter::new();
    let mut reply_limit_warned = false;
    let mut snapshot_schedule = OutputSnapshotSchedule::default();
    enum Incoming {
        Command(SessionCommand),
        Pty(PtyEvent),
        SnapshotDeadline,
    }
    loop {
        let now = std::time::Instant::now();
        if snapshot_schedule.is_due(now) && !engine.synchronized_output() {
            publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
        }
        let incoming = if let Ok(command) = commands.try_recv() {
            // Commands are latency-sensitive and must not be starved by a PTY
            // whose output receiver remains continuously ready.
            Some(Incoming::Command(command))
        } else {
            let selector = flume::Selector::new()
                .recv(&commands, |value| value.ok().map(Incoming::Command))
                .recv(pty.events(), |value| value.ok().map(Incoming::Pty));
            if engine.synchronized_output() {
                selector.wait()
            } else if let Some(deadline) = snapshot_schedule.deadline() {
                selector
                    .wait_deadline(deadline)
                    .unwrap_or(Some(Incoming::SnapshotDeadline))
            } else {
                selector.wait()
            }
        };
        let Some(incoming) = incoming else { return };
        match incoming {
            Incoming::Command(SessionCommand::Ssh {
                method,
                params,
                reply,
                deadline,
            }) => {
                if std::time::Instant::now() >= deadline {
                    let _ = reply.send(Err("SSH request expired before submission".into()));
                    continue;
                }
                let result = ssh.request(&method, &params).and_then(|line| {
                    if let Some(line) = line {
                        let bytes = if method == "ssh.cancel" {
                            vec![3]
                        } else {
                            ssh.prepare_dispatch(line);
                            ssh::DISPATCH_PROBE.to_vec()
                        };
                        pty.send(PtyCommand::Input(bytes))
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(ssh.status())
                });
                let _ = reply.send(result);
            }
            Incoming::SnapshotDeadline => continue,
            Incoming::Command(SessionCommand::Input(bytes)) => {
                if !bytes.is_empty() {
                    ssh.user_input();
                }
                if engine.scroll_to_bottom() {
                    publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
                }
                let _ = pty.send(PtyCommand::Input(bytes));
            }
            Incoming::Command(SessionCommand::Key(input)) => {
                if let Ok(bytes) = engine.encode_key(&input) {
                    if !bytes.is_empty() {
                        ssh.user_input();
                    }
                    if engine.scroll_to_bottom() {
                        publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
                    }
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::KeyRelease(input)) => {
                if let Ok(bytes) = engine.encode_key_release(&input)
                    && !bytes.is_empty()
                {
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::Focus(focused)) => {
                if let Ok(bytes) = engine.encode_focus(focused)
                    && !bytes.is_empty()
                {
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::Paste(text)) => {
                if !text.is_empty() {
                    ssh.user_input();
                }
                if let Ok(bytes) = engine.encode_paste(&text) {
                    if engine.scroll_to_bottom() {
                        publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
                    }
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::PasteLine(text)) => {
                ssh.user_input();
                if let Ok(mut bytes) = engine.encode_paste(&text) {
                    bytes.push(b'\r');
                    if engine.scroll_to_bottom() {
                        publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
                    }
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::CopyAll) => match engine.select_all_text() {
                Ok(text) => {
                    let _ = events.send(SessionEvent::ClipboardText {
                        text,
                        target: ClipboardTarget::Clipboard,
                    });
                }
                Err(error) => {
                    let _ = events.send(SessionEvent::Warning(format!("copy all: {error}")));
                }
            },
            Incoming::Command(SessionCommand::CopySelection { start, end, target }) => match engine
                .selection_text(start, end)
            {
                Ok(text) => {
                    let _ = events.send(SessionEvent::ClipboardText { text, target });
                }
                Err(error) => {
                    let _ = events.send(SessionEvent::Warning(format!("copy selection: {error}")));
                }
            },
            Incoming::Command(SessionCommand::Mouse(input)) => match engine.encode_mouse(input) {
                Ok(bytes) if !bytes.is_empty() => {
                    ssh.user_input();
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = events.send(SessionEvent::Warning(format!("mouse input: {error}")));
                }
            },
            Incoming::Command(SessionCommand::Resize {
                size,
                cell_width_px,
                cell_height_px,
            }) => {
                if let Err(error) = engine.resize(size, cell_width_px, cell_height_px) {
                    let _ = events.send(SessionEvent::Warning(format!("resize: {error}")));
                }
                let _ = pty.send(PtyCommand::Resize(PtySize::new(size.cols, size.rows)));
            }
            Incoming::Command(SessionCommand::Scroll(lines)) => {
                engine.scroll(lines);
                publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
            }
            Incoming::Command(SessionCommand::Search { query, direction }) => {
                match engine.search(&query, direction) {
                    Ok(result) => {
                        let _ = events.send(SessionEvent::SearchResult(result));
                        publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
                    }
                    Err(error) => {
                        let _ = events.send(SessionEvent::Warning(format!("search: {error}")));
                    }
                }
            }
            Incoming::Command(SessionCommand::ClearScrollback) => {
                engine.clear_scrollback();
                publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
            }
            Incoming::Command(SessionCommand::ExportHistory(sender)) => {
                let _ = sender.send(engine.select_all_text().unwrap_or_default());
            }
            Incoming::Command(SessionCommand::RestoreHistory(text)) => {
                engine.feed(&normalize_restored_history(&text));
                let _ = engine.take_effects();
                publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
            }
            Incoming::Command(SessionCommand::SetTheme(theme)) => {
                if let Err(error) = engine.apply_theme(&theme) {
                    let _ = events.send(SessionEvent::Warning(format!("apply theme: {error}")));
                } else {
                    publish_snapshot(&mut engine, &events, &mut snapshot_schedule, false);
                }
            }
            Incoming::Command(SessionCommand::Shutdown) => return,
            Incoming::Pty(PtyEvent::Output(bytes)) => {
                ssh.feed(&bytes);
                if let Some(bytes) = ssh.take_dispatch_output() {
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
                process_pty_output_chunk(
                    &bytes,
                    &mut engine,
                    &mut osc,
                    &pty,
                    &events,
                    &mut reply_limiter,
                    &mut reply_limit_warned,
                );
                snapshot_schedule.mark_dirty(std::time::Instant::now());
            }
            Incoming::Pty(PtyEvent::Exited(status)) => {
                if snapshot_schedule.is_dirty() {
                    // Output and process-exit are separate PTY events. Publish
                    // the final grid first even when the process exits inside
                    // an unterminated synchronized-output transaction.
                    publish_snapshot(&mut engine, &events, &mut snapshot_schedule, true);
                }
                let _ = events.send(SessionEvent::Exited(status.to_string()));
                return;
            }
            Incoming::Pty(PtyEvent::ForegroundProcess(commands)) => {
                let _ = events.send(SessionEvent::ForegroundProcess(commands));
            }
            Incoming::Pty(PtyEvent::ChildProcesses(commands)) => {
                let _ = events.send(SessionEvent::ChildProcesses(commands));
            }
            Incoming::Pty(PtyEvent::Error(error)) => {
                let _ = events.send(SessionEvent::Error(error));
                return;
            }
        }
    }
}

fn normalize_restored_history(text: &str) -> Vec<u8> {
    let mut normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    if !normalized.is_empty() && !normalized.ends_with("\r\n") {
        normalized.push_str("\r\n");
    }
    normalized.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_history_is_crlf_normalized_and_ends_before_the_new_prompt() {
        assert_eq!(
            normalize_restored_history("old prompt$"),
            b"old prompt$\r\n"
        );
        assert_eq!(normalize_restored_history("one\ntwo\n"), b"one\r\ntwo\r\n");
        assert!(normalize_restored_history("").is_empty());
    }

    #[test]
    fn parses_cursor_movement_and_sgr_without_leaking_ansi() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(12, 3).unwrap()).unwrap();
        terminal.feed(b"hello\r\n\x1b[31mworld\x1b[0m\x1b[1;1HX");

        let snapshot = terminal.snapshot().unwrap();
        assert_eq!(snapshot.lines[0], "Xello");
        assert_eq!(snapshot.lines[1], "world");
        assert_eq!(snapshot.to_plain_text(), "Xello\nworld");
    }

    #[test]
    fn snapshot_preserves_and_ends_osc8_hyperlinks() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(12, 2).unwrap()).unwrap();
        terminal.feed(b"\x1b]8;;https://example.com/path\x07link\x1b]8;;\x07 plain");

        let snapshot = terminal.snapshot().unwrap();
        let linked = &snapshot.cells[0][..4];
        assert!(
            linked
                .iter()
                .all(|cell| { cell.hyperlink.as_deref() == Some("https://example.com/path") })
        );
        assert!(Arc::ptr_eq(
            linked[0].hyperlink.as_ref().unwrap(),
            linked[1].hyperlink.as_ref().unwrap()
        ));
        assert!(snapshot.cells[0][5].hyperlink.is_none());
    }

    #[test]
    fn synchronized_output_mode_brackets_atomic_screen_updates() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(12, 3).unwrap()).unwrap();
        assert!(!terminal.synchronized_output());
        terminal.feed(b"\x1b[?2026hWorking");
        assert!(terminal.synchronized_output());
        terminal.feed(b"\x1b[?2026l");
        assert!(!terminal.synchronized_output());
        assert_eq!(terminal.snapshot().unwrap().lines[0], "Working");
    }

    #[test]
    fn output_snapshot_deadline_is_bounded_by_the_first_chunk() {
        let start = std::time::Instant::now();
        let mut schedule = OutputSnapshotSchedule::default();
        schedule.mark_dirty(start);
        let deadline = schedule.deadline().unwrap();
        schedule.mark_dirty(start + std::time::Duration::from_millis(15));
        assert_eq!(schedule.deadline(), Some(deadline));
        assert!(!schedule.is_due(deadline - std::time::Duration::from_nanos(1)));
        assert!(schedule.is_due(deadline));
        schedule.mark_presented();
        assert!(!schedule.is_dirty());
    }

    #[test]
    fn resize_updates_snapshot_dimensions() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        terminal
            .resize(TerminalSize::new(20, 4).unwrap(), 8, 16)
            .unwrap();
        let snapshot = terminal.snapshot().unwrap();
        assert_eq!((snapshot.cols, snapshot.rows), (20, 4));
    }

    #[test]
    fn paste_normalizes_windows_line_endings_without_inserting_blank_lines() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(20, 2).unwrap()).unwrap();
        let text = "\u{4e2d}\u{6587}\r\n\u{1f642}\rfinal\n";
        assert_eq!(
            terminal.encode_paste(text).unwrap(),
            "\u{4e2d}\u{6587}\r\u{1f642}\rfinal\r".as_bytes()
        );
        terminal.feed(b"\x1b[?2004h");
        assert_eq!(
            terminal.encode_paste(text).unwrap(),
            "\x1b[200~\u{4e2d}\u{6587}\n\u{1f642}\rfinal\n\x1b[201~".as_bytes()
        );
    }

    #[test]
    fn key_encoder_tracks_terminal_application_cursor_mode() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        let input = KeyInput {
            key: InputKey::Up,
            text: None,
            modifiers: KeyModifiers::default(),
            repeat: false,
            unshifted_codepoint: None,
        };
        assert_eq!(terminal.encode_key(&input).unwrap(), b"\x1b[A");
        terminal.feed(b"\x1b[?1h");
        assert_eq!(terminal.encode_key(&input).unwrap(), b"\x1bOA");
    }

    #[test]
    fn space_key_encodes_as_a_printable_terminal_space() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        let input = KeyInput {
            key: InputKey::Space,
            text: Some(" ".into()),
            modifiers: KeyModifiers::default(),
            repeat: false,
            unshifted_codepoint: None,
        };
        assert_eq!(terminal.encode_key(&input).unwrap(), b" ");
    }

    #[test]
    fn control_keys_ignore_platform_text_with_enhanced_keyboard_protocol() {
        for enhanced_keyboard in [false, true] {
            let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
            if enhanced_keyboard {
                // Codex and similar TUIs enable Kitty keyboard disambiguation.
                terminal.feed(b"\x1b[>1u");
            }
            for (key, text, expected) in [
                (InputKey::Character('a'), "a", 0x01),
                (InputKey::Character('c'), "\u{3}", 0x03),
                (InputKey::Character('c'), "c", 0x03),
                (InputKey::Character('d'), "\u{4}", 0x04),
                (InputKey::Character('t'), "t", 0x14),
                (InputKey::Character('z'), "z", 0x1a),
                (InputKey::Character('['), "[", 0x1b),
                (InputKey::Character('\\'), "\\", 0x1c),
                (InputKey::Character(']'), "]", 0x1d),
                (InputKey::Character('^'), "^", 0x1e),
                (InputKey::Character('_'), "_", 0x1f),
                (InputKey::Character('?'), "?", 0x7f),
                (InputKey::Space, " ", 0x00),
            ] {
                let input = KeyInput {
                    key,
                    text: Some(text.into()),
                    modifiers: KeyModifiers {
                        control: true,
                        ..KeyModifiers::default()
                    },
                    repeat: false,
                    unshifted_codepoint: None,
                };
                assert_eq!(
                    terminal.encode_key(&input).unwrap(),
                    if enhanced_keyboard {
                        let code = match key {
                            InputKey::Character(c) => c as u32,
                            InputKey::Space => 32,
                            _ => unreachable!(),
                        };
                        format!("\x1b[{code};5u").into_bytes()
                    } else {
                        vec![expected]
                    },
                    "enhanced_keyboard={enhanced_keyboard}, key={key:?}"
                );
            }
        }
    }

    #[test]
    fn snapshot_preserves_subtle_and_inverse_backgrounds() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        terminal.feed(b"\x1b[48;2;1;2;3mA\x1b[0m\x1b[7mB\x1b[0mC");

        let snapshot = terminal.snapshot().unwrap();
        assert!(snapshot.cells[0][0].background_explicit);
        assert_eq!(snapshot.cells[0][0].background, Rgb { r: 1, g: 2, b: 3 });
        assert!(snapshot.cells[0][1].background_explicit);
        assert_eq!(
            snapshot.cells[0][1].background,
            snapshot.cells[0][2].foreground
        );
        assert_eq!(
            snapshot.cells[0][1].foreground,
            snapshot.cells[0][2].background
        );
    }

    #[test]
    fn configured_theme_answers_foreground_and_background_queries() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        let theme = TerminalTheme {
            foreground: Rgb {
                r: 205,
                g: 214,
                b: 244,
            },
            background: Rgb {
                r: 30,
                g: 30,
                b: 46,
            },
            cursor: Rgb {
                r: 203,
                g: 166,
                b: 247,
            },
            palette: Vec::new(),
        };
        terminal.apply_theme(&theme).unwrap();
        assert_eq!(
            terminal.terminal.default_bg_color().unwrap(),
            Some(RgbColor {
                r: 30,
                g: 30,
                b: 46
            })
        );
        terminal.feed(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n");
        let replies = terminal
            .take_effects()
            .into_iter()
            .filter_map(|effect| match effect {
                TerminalEffect::PtyWrite(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        let replies = String::from_utf8(replies).unwrap();
        assert!(replies.contains("rgb:cdcd/d6d6/f4f4"), "{replies:?}");
        assert!(replies.contains("rgb:1e1e/1e1e/2e2e"), "{replies:?}");
        assert!(replies.contains("\x1b[?997;1n"), "{replies:?}");
    }

    #[test]
    fn mouse_encoder_tracks_terminal_sgr_mode() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        assert!(!terminal.snapshot().unwrap().mouse_tracking);
        terminal.feed(b"\x1b[?1000h\x1b[?1006h");
        assert!(terminal.snapshot().unwrap().mouse_tracking);
        let bytes = terminal
            .encode_mouse(MouseInput {
                action: MouseActionInput::Press,
                button: Some(MouseButtonInput::Left),
                x: 8.0,
                y: 16.0,
                cell_width: 8,
                cell_height: 16,
                modifiers: KeyModifiers::default(),
                any_button_pressed: true,
            })
            .unwrap();
        assert_eq!(bytes, b"\x1b[<0;2;2M");
    }

    #[test]
    fn focus_and_key_release_follow_negotiated_modes() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        let input = KeyInput {
            key: InputKey::Character('g'),
            text: Some("g".into()),
            modifiers: KeyModifiers {
                control: true,
                ..Default::default()
            },
            repeat: false,
            unshifted_codepoint: None,
        };
        assert!(terminal.encode_focus(true).unwrap().is_empty());
        assert!(terminal.encode_key_release(&input).unwrap().is_empty());
        terminal.feed(b"\x1b[?1004h\x1b[>7u");
        assert_eq!(terminal.encode_focus(true).unwrap(), b"\x1b[I");
        assert_eq!(terminal.encode_focus(false).unwrap(), b"\x1b[O");
        assert_eq!(terminal.encode_key(&input).unwrap(), b"\x1b[103;5u");
        assert_eq!(
            terminal.encode_key_release(&input).unwrap(),
            b"\x1b[103;5:3u"
        );
        let repeat = KeyInput {
            repeat: true,
            ..input.clone()
        };
        assert_eq!(terminal.encode_key(&repeat).unwrap(), b"\x1b[103;5:2u");
        terminal.feed(b"\x1b[?1004l\x1b[<u");
        assert!(terminal.encode_focus(false).unwrap().is_empty());
        assert!(terminal.encode_key_release(&input).unwrap().is_empty());
        assert_eq!(terminal.encode_key(&input).unwrap(), [7]);
        terminal.feed(b"\x1b[>1u");
        for number in 1..=24 {
            let input = KeyInput {
                key: InputKey::Function(number),
                text: None,
                modifiers: Default::default(),
                repeat: false,
                unshifted_codepoint: None,
            };
            assert!(
                !terminal.encode_key(&input).unwrap().is_empty(),
                "F{number}"
            );
        }
    }

    #[test]
    fn shifted_punctuation_uses_the_layout_base_character() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        terminal.feed(b"\x1b[>7u");
        let input = KeyInput {
            key: InputKey::Character('?'),
            unshifted_codepoint: Some('/'),
            text: Some("?".into()),
            modifiers: KeyModifiers {
                shift: true,
                ..Default::default()
            },
            repeat: false,
        };
        assert_eq!(terminal.encode_key(&input).unwrap(), b"\x1b[47:63;2u");
        let controlled = KeyInput {
            modifiers: KeyModifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
            ..input
        };
        assert_eq!(terminal.encode_key(&controlled).unwrap(), b"\x1b[47;6u");
    }

    #[test]
    fn mouse_hover_and_button_release_follow_application_modes() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        let mut input = MouseInput {
            action: MouseActionInput::Motion,
            button: None,
            x: 8.0,
            y: 16.0,
            cell_width: 8,
            cell_height: 16,
            modifiers: Default::default(),
            any_button_pressed: false,
        };
        terminal.feed(b"\x1b[?1002h\x1b[?1006h");
        assert!(terminal.encode_mouse(input).unwrap().is_empty());
        terminal.feed(b"\x1b[?1003h");
        input.x = 16.0;
        assert_eq!(terminal.encode_mouse(input).unwrap(), b"\x1b[<35;3;2M");
        for (button, code) in [(MouseButtonInput::Right, 2), (MouseButtonInput::Middle, 1)] {
            input.button = Some(button);
            input.action = MouseActionInput::Press;
            input.any_button_pressed = true;
            assert_eq!(
                terminal.encode_mouse(input).unwrap(),
                format!("\x1b[<{code};3;2M").into_bytes()
            );
            input.action = MouseActionInput::Release;
            input.any_button_pressed = false;
            assert_eq!(
                terminal.encode_mouse(input).unwrap(),
                format!("\x1b[<{code};3;2m").into_bytes()
            );
        }
    }

    #[test]
    fn reports_vt_side_effects() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        terminal.feed(b"\x07\x1b]2;Build\x1b\\");
        let effects = terminal.take_effects();
        assert!(effects.contains(&TerminalEffect::Bell));
        assert!(effects.contains(&TerminalEffect::TitleChanged("Build".into())));
    }

    #[test]
    fn protocol_reply_batches_are_bounded_and_fully_drained() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(80, 24).unwrap()).unwrap();
        let queries = b"\x1b[c".repeat(4096);
        terminal.feed(&queries);

        let mut limiter = ProtocolReplyLimiter::new();
        limiter.begin_output_chunk(std::time::Instant::now());
        let drained = drain_terminal_effects(&terminal, &mut limiter);
        assert!(drained.reply_truncated);
        assert!(drained.protocol_reply.len() <= MAX_PROTOCOL_REPLY_BYTES_PER_BURST);
        assert!(drained.remaining.is_empty());
        assert!(terminal.take_effects().is_empty());
    }

    #[test]
    fn protocol_reply_budget_recovers_after_output_is_quiet() {
        let start = std::time::Instant::now();
        let mut limiter = ProtocolReplyLimiter::new();
        limiter.begin_output_chunk(start);
        assert!(limiter.accept(MAX_PROTOCOL_REPLY_BYTES_PER_BURST));
        assert!(!limiter.accept(1));

        limiter.begin_output_chunk(start + PROTOCOL_REPLY_BURST_QUIET);
        assert!(limiter.accept(1));
    }

    #[test]
    fn formats_screen_selection_for_clipboard() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        terminal.feed(b"copy me");
        assert!(terminal.select_all_text().unwrap().contains("copy me"));
    }

    #[test]
    fn formats_absolute_selection_after_viewport_scroll() {
        let mut terminal =
            TerminalEngine::new_with_scrollback(TerminalSize::new(10, 2).unwrap(), 100).unwrap();
        terminal.feed(b"first\r\nsecond\r\nthird");
        assert!(terminal.snapshot().unwrap().scrollback_rows > 0);
        terminal.scroll(-1);
        let text = terminal
            .selection_text(
                SelectionPoint {
                    column: 0,
                    screen_row: 0,
                },
                SelectionPoint {
                    column: 4,
                    screen_row: 0,
                },
            )
            .unwrap();
        assert_eq!(text, "first");
    }

    #[test]
    fn preserves_cjk_emoji_and_combining_graphemes() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(30, 2).unwrap()).unwrap();
        terminal.feed("中文 🙂 e\u{301}".as_bytes());
        let snapshot = terminal.snapshot().unwrap();
        let text = snapshot.to_plain_text();
        assert!(text.contains("中文"));
        assert!(text.contains('🙂'));
        assert!(text.contains("e\u{301}"));
        assert_eq!(snapshot.cells[0][0].text, "中");
        assert_eq!(snapshot.cells[0][1].text, "");
        assert_eq!(snapshot.cells[0][2].text, "文");
        assert_eq!(snapshot.cells[0][3].text, "");

        let cjk = terminal
            .selection_text(
                SelectionPoint {
                    column: 0,
                    screen_row: 0,
                },
                SelectionPoint {
                    column: 3,
                    screen_row: 0,
                },
            )
            .unwrap();
        assert_eq!(cjk, "中文");

        let continuation_cell = terminal.selection_text(
            SelectionPoint {
                column: 1,
                screen_row: 0,
            },
            SelectionPoint {
                column: 1,
                screen_row: 0,
            },
        );
        assert!(
            continuation_cell.is_ok(),
            "selecting a wide-glyph continuation cell must not fail: {continuation_cell:?}"
        );

        let clamped = terminal
            .selection_text(
                SelectionPoint {
                    column: 0,
                    screen_row: 0,
                },
                SelectionPoint {
                    column: u16::MAX,
                    screen_row: u32::MAX,
                },
            )
            .unwrap();
        assert!(clamped.contains("中文"));
    }

    #[test]
    fn restores_primary_screen_after_alternate_screen() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(20, 2).unwrap()).unwrap();
        terminal.feed(b"primary\x1b[?1049hAlternate\x1b[?1049l");
        assert!(
            terminal
                .snapshot()
                .unwrap()
                .to_plain_text()
                .contains("primary")
        );
    }

    #[test]
    fn parses_high_throughput_output_into_bounded_viewport() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(80, 24).unwrap()).unwrap();
        let output = (0..5_000)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        terminal.feed(output.as_bytes());
        let snapshot = terminal.snapshot().unwrap();
        assert_eq!(snapshot.lines.len(), 24);
        assert!(snapshot.scrollback_rows >= 4_900);
        assert!(snapshot.to_plain_text().contains("line 4999"));
    }

    #[test]
    fn ongoing_output_keeps_scrolled_viewport_offset_reachable_from_bottom() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(20, 3).unwrap()).unwrap();
        terminal.feed(b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\n");
        terminal.scroll(-2);
        let before = terminal.snapshot().unwrap();
        assert_eq!(before.viewport_offset, 2);

        terminal.feed(b"line-5\r\nline-6\r\n");
        let during = terminal.snapshot().unwrap();
        assert_eq!(during.viewport_offset, 4);

        terminal.scroll(isize::MAX);
        let bottom = terminal.snapshot().unwrap();
        assert_eq!(bottom.viewport_offset, 0);
        assert!(bottom.to_plain_text().contains("line-6"));
    }

    #[test]
    fn terminal_input_can_return_a_scrolled_viewport_to_live_output() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(20, 3).unwrap()).unwrap();
        terminal.feed(b"line-0\r\nline-1\r\nline-2\r\nline-3\r\nline-4\r\n");
        terminal.scroll(-2);
        assert_eq!(terminal.snapshot().unwrap().viewport_offset, 2);

        assert!(terminal.scroll_to_bottom());
        let bottom = terminal.snapshot().unwrap();
        assert_eq!(bottom.viewport_offset, 0);
        assert!(bottom.to_plain_text().contains("line-4"));
        assert!(!terminal.scroll_to_bottom());
    }

    #[test]
    fn logical_scrollback_limit_bounds_scroll_search_and_copy_all() {
        let mut terminal = TerminalEngine::new_with_scrollback_limits(
            TerminalSize::new(20, 2).unwrap(),
            DEFAULT_SCROLLBACK_BYTES,
            3,
        )
        .unwrap();
        terminal.feed(
            (0..10)
                .map(|line| format!("bounded-{line}\r\n"))
                .collect::<String>()
                .as_bytes(),
        );

        let bottom = terminal.snapshot().unwrap();
        assert_eq!(bottom.scrollback_rows, 3);
        assert_eq!(terminal.search("bounded-0", 1).unwrap().total, 0);
        assert_eq!(terminal.search("bounded-9", 1).unwrap().total, 1);
        let copied = terminal.select_all_text().unwrap();
        assert!(!copied.contains("bounded-0"));
        assert!(copied.contains("bounded-9"));

        terminal.scroll(-100);
        let top = terminal.snapshot().unwrap();
        assert_eq!(top.viewport_offset, 3);
        assert_eq!(
            bottom
                .screen_row_origin
                .saturating_sub(top.screen_row_origin),
            3
        );
    }

    #[test]
    fn searches_full_scrollback_and_clears_it() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(20, 3).unwrap()).unwrap();
        terminal.feed(b"needle first\r\nsecond\r\nthird\r\nneedle last\r\n");
        let first = terminal.search("needle", 1).unwrap();
        assert_eq!((first.current, first.total), (1, 2));
        let second = terminal.search("needle", 1).unwrap();
        assert_eq!((second.current, second.total), (2, 2));
        terminal.clear_scrollback();
        assert_eq!(terminal.search("needle", 1).unwrap().total, 0);
    }

    #[test]
    #[cfg(unix)]
    fn terminal_actor_owns_pty_and_emulator_state() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args(["-lc", "printf '\\033[32mactor-ok\\033[0m\\n'"]),
            TerminalSize::new(40, 4).unwrap(),
        )
        .unwrap();
        let mut found = false;
        while let Ok(event) = session
            .events()
            .recv_timeout(std::time::Duration::from_secs(2))
        {
            match event {
                SessionEvent::Snapshot(snapshot) => {
                    found |= snapshot.to_plain_text().contains("actor-ok")
                }
                SessionEvent::Exited(_) => break,
                SessionEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(found);
    }

    #[test]
    #[cfg(unix)]
    fn terminal_actor_publishes_final_synchronized_output_before_exit() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args(["-lc", r"printf '\033[?2026hFINAL-SYNC'"]),
            TerminalSize::new(40, 4).unwrap(),
        )
        .unwrap();
        let mut screens = Vec::new();
        while let Ok(event) = session
            .events()
            .recv_timeout(std::time::Duration::from_secs(2))
        {
            match event {
                SessionEvent::Snapshot(snapshot) => {
                    screens.push(snapshot.to_plain_text());
                }
                SessionEvent::Exited(_) => break,
                SessionEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        let final_screen = screens.last().expect("final snapshot before exit");
        assert!(final_screen.contains("FINAL-SYNC"));
        assert!(
            screens[..screens.len() - 1]
                .iter()
                .all(|screen| !screen.contains("FINAL-SYNC")),
            "synchronized output was presented partially"
        );
    }

    #[test]
    #[cfg(unix)]
    fn terminal_actor_services_commands_while_output_snapshot_is_deferred() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args([
                "-lc",
                r"printf '\033[?2026hbusy'; sleep 1; printf '\033[?2026l'",
            ]),
            TerminalSize::new(40, 4).unwrap(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let started = std::time::Instant::now();
        assert!(session.export_history().is_some());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "actor command was starved by PTY output"
        );
    }

    #[test]
    #[cfg(unix)]
    fn paste_line_submits_text_and_enter_as_one_actor_command() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args(["-lc", "read value; printf 'line=%s\\n' \"$value\""]),
            TerminalSize::new(40, 4).unwrap(),
        )
        .unwrap();
        session
            .send(SessionCommand::PasteLine("atomic input".into()))
            .unwrap();
        let mut found = false;
        while let Ok(event) = session
            .events()
            .recv_timeout(std::time::Duration::from_secs(2))
        {
            match event {
                SessionEvent::Snapshot(snapshot) => {
                    found |= snapshot.to_plain_text().contains("line=atomic input")
                }
                SessionEvent::Exited(_) => break,
                SessionEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(found);
    }

    #[test]
    #[cfg(unix)]
    fn long_paste_followed_by_separate_enter_preserves_pty_byte_order() {
        const PASTE_LEN: usize = 256 * 1024;
        let command = "stty raw -echo; printf '\\033[?2004hREADY'; \
             prefix=$(dd bs=6 count=1 iflag=fullblock 2>/dev/null | od -An -tx1); \
             dd bs=4096 count=64 iflag=fullblock 2>/dev/null >/dev/null; \
             suffix=$(dd bs=7 count=1 iflag=fullblock 2>/dev/null | od -An -tx1); \
             stty sane; printf '\\r\\nprefix=%s suffix=%s\\r\\n' \"$prefix\" \"$suffix\"";
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args(["-lc", command]),
            TerminalSize::new(80, 4).unwrap(),
        )
        .unwrap();

        let mut ready = false;
        while let Ok(event) = session
            .events()
            .recv_timeout(std::time::Duration::from_secs(2))
        {
            match event {
                SessionEvent::Snapshot(snapshot) if snapshot.to_plain_text().contains("READY") => {
                    ready = true;
                    break;
                }
                SessionEvent::Error(error) => panic!("{error}"),
                SessionEvent::Exited(status) => panic!("PTY exited before paste: {status}"),
                _ => {}
            }
        }
        assert!(ready, "PTY did not enable bracketed paste in time");

        session
            .send(SessionCommand::Paste("x".repeat(PASTE_LEN)))
            .unwrap();
        session
            .send(SessionCommand::Key(KeyInput {
                key: InputKey::Enter,
                text: None,
                modifiers: KeyModifiers::default(),
                repeat: false,
                unshifted_codepoint: None,
            }))
            .unwrap();

        let mut output = String::new();
        while let Ok(event) = session
            .events()
            .recv_timeout(std::time::Duration::from_secs(5))
        {
            match event {
                SessionEvent::Snapshot(snapshot) => {
                    output = snapshot.to_plain_text();
                    if output.contains("suffix=") {
                        break;
                    }
                }
                SessionEvent::Error(error) => panic!("{error}"),
                SessionEvent::Exited(_) => break,
                _ => {}
            }
        }
        assert!(
            output.contains("prefix= 1b 5b 32 30 30 7e"),
            "missing bracketed-paste prefix: {output:?}"
        );
        assert!(
            output.contains("suffix= 1b 5b 32 30 31 7e 0d"),
            "Enter did not immediately follow the paste suffix: {output:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn protocol_reply_flood_does_not_displace_the_final_process_output() {
        let session = TerminalSession::spawn(
            CommandSpec::new("sh").args([
                "-lc",
                r"i=0; while [ $i -lt 5000 ]; do printf '\033[c'; i=$((i + 1)); done; printf '\r\nFINAL-MARKER\r\n'; sleep 1",
            ]),
            TerminalSize::new(80, 24).unwrap(),
        )
        .unwrap();
        let mut final_screen = String::new();
        while let Ok(event) = session
            .events()
            .recv_timeout(std::time::Duration::from_secs(3))
        {
            match event {
                SessionEvent::Snapshot(snapshot) => final_screen = snapshot.to_plain_text(),
                SessionEvent::Exited(_) => break,
                SessionEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(
            final_screen.contains("FINAL-MARKER"),
            "protocol replies displaced output emitted after the query flood: {final_screen:?}"
        );
    }
}
