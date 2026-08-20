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
use libghostty_vt::terminal::{ColorScheme, Point, PointCoordinate, ScrollViewport};
use libghostty_vt::{RenderState, Terminal, TerminalOptions};
use thiserror::Error;
use wmcterm_agent::{AgentEvent, OscEvent, OscScanner};
use wmcterm_pty::{CommandSpec, PtyCommand, PtyEvent, PtySession, PtySize};

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

        while let Some(row) = row_iter.next() {
            let mut line = String::new();
            let mut row_cells = Vec::with_capacity(cols as usize);
            let mut cell_iter = self.cells.update(row)?;
            while let Some(cell) = cell_iter.next() {
                let graphemes = cell.graphemes()?;
                let wide = cell.raw_cell()?.wide()?;
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
                    foreground,
                    background,
                    background_explicit: background_color.is_some() || style.inverse,
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                    strikethrough: style.strikethrough,
                });
            }
            lines.push(line.trim_end().to_owned());
            all_cells.push(row_cells);
        }
        let actual_scrollback_rows = self.terminal.scrollback_rows()?;
        Ok(ScreenSnapshot {
            cols,
            rows,
            lines,
            cells: all_cells,
            cursor,
            mouse_tracking: self.terminal.is_mouse_tracking()?,
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
        let start = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: start.column,
            y: start.screen_row,
        }))?;
        let end = self.terminal.grid_ref(Point::Screen(PointCoordinate {
            x: end.column,
            y: end.screen_row,
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
        let mut input = text.as_bytes().to_vec();
        let mut output = vec![0_u8; input.len() + 16];
        let written = libghostty_vt::paste::encode(&mut input, false, &mut output)?;
        output.truncate(written);
        Ok(output)
    }

    pub fn encode_key(&mut self, input: &KeyInput) -> Result<Vec<u8>, TerminalError> {
        // These conventional terminal controls must remain raw bytes even when
        // a TUI enables an enhanced keyboard protocol. This also avoids
        // platform text fields containing the already-transformed C0 value.
        if input.modifiers.control
            && !input.modifiers.shift
            && !input.modifiers.alt
            && !input.modifiers.super_key
        {
            match input.key {
                InputKey::Character('c' | 'C') => return Ok(vec![0x03]),
                InputKey::Character('d' | 'D') => return Ok(vec![0x04]),
                _ => {}
            }
        }
        let mut event = KeyEvent::new()?;
        event
            .set_action(if input.repeat {
                KeyAction::Repeat
            } else {
                KeyAction::Press
            })
            .set_key(input.key.into())
            .set_mods(input.modifiers.into());
        if let Some(text) = input.text.as_ref().filter(|text| valid_key_text(text)) {
            event.set_utf8(Some(text.clone()));
        }
        self.key_encoder.set_options_from_terminal(&self.terminal);
        let mut bytes = Vec::with_capacity(16);
        self.key_encoder.encode_to_vec(&event, &mut bytes)?;
        Ok(bytes)
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
    Input(Vec<u8>),
    Key(KeyInput),
    Paste(String),
    CopyAll,
    CopySelection {
        start: SelectionPoint,
        end: SelectionPoint,
    },
    Mouse(MouseInput),
    Resize(TerminalSize),
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
    Notification(String),
    ClipboardText(String),
    SearchResult(SearchResult),
    Exited(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub query: String,
    pub current: usize,
    pub total: usize,
}

pub struct TerminalSession {
    commands: flume::Sender<SessionCommand>,
    events: flume::Receiver<SessionEvent>,
    worker: Option<JoinHandle<()>>,
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
                )
            })
            .map_err(|error| TerminalError::Runtime(error.to_string()))?;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            worker: Some(worker),
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
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.commands.send(SessionCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const MAX_PTY_OUTPUT_CHUNKS_PER_BATCH: usize = 64;
const MAX_PROTOCOL_REPLY_BYTES_PER_BURST: usize = 1024;
const PROTOCOL_REPLY_BURST_QUIET: std::time::Duration = std::time::Duration::from_millis(250);

struct ProtocolReplyLimiter {
    remaining: usize,
    last_output: Option<std::time::Instant>,
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

fn run_terminal_actor(
    pty: PtySession,
    size: TerminalSize,
    scrollback: ScrollbackLimits,
    history: Option<String>,
    theme: Option<TerminalTheme>,
    commands: flume::Receiver<SessionCommand>,
    events: flume::Sender<SessionEvent>,
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
        let _ = events.send(SessionEvent::Error(error.to_string()));
        return;
    }
    if let Some(history) = history.filter(|history| !history.is_empty()) {
        engine.feed(&normalize_restored_history(&history));
        // Persisted output is render-only replay. Never send terminal replies,
        // bells, or title changes back into a newly spawned PTY.
        let _ = engine.take_effects();
        if let Ok(snapshot) = engine.snapshot() {
            let _ = events.send(SessionEvent::Snapshot(snapshot));
        }
    }
    let mut osc = OscScanner::default();
    let mut reply_limiter = ProtocolReplyLimiter::new();
    let mut reply_limit_warned = false;
    enum Incoming {
        Command(SessionCommand),
        Pty(PtyEvent),
    }
    let mut pending_pty = None;
    loop {
        let incoming = if let Some(event) = pending_pty.take() {
            Some(Incoming::Pty(event))
        } else {
            flume::Selector::new()
                .recv(&commands, |value| value.ok().map(Incoming::Command))
                .recv(pty.events(), |value| value.ok().map(Incoming::Pty))
                .wait()
        };
        let Some(incoming) = incoming else { return };
        match incoming {
            Incoming::Command(SessionCommand::Input(bytes)) => {
                let _ = pty.send(PtyCommand::Input(bytes));
            }
            Incoming::Command(SessionCommand::Key(input)) => {
                if let Ok(bytes) = engine.encode_key(&input) {
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::Paste(text)) => {
                if let Ok(bytes) = engine.encode_paste(&text) {
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
            }
            Incoming::Command(SessionCommand::CopyAll) => {
                if let Ok(text) = engine.select_all_text() {
                    let _ = events.send(SessionEvent::ClipboardText(text));
                }
            }
            Incoming::Command(SessionCommand::CopySelection { start, end }) => {
                match engine.selection_text(start, end) {
                    Ok(text) => {
                        let _ = events.send(SessionEvent::ClipboardText(text));
                    }
                    Err(error) => {
                        let _ = events.send(SessionEvent::Error(error.to_string()));
                    }
                }
            }
            Incoming::Command(SessionCommand::Mouse(input)) => match engine.encode_mouse(input) {
                Ok(bytes) if !bytes.is_empty() => {
                    let _ = pty.send(PtyCommand::Input(bytes));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(error.to_string()));
                }
            },
            Incoming::Command(SessionCommand::Resize(size)) => {
                if let Err(error) = engine.resize(size, 10, 21) {
                    let _ = events.send(SessionEvent::Error(error.to_string()));
                }
                let _ = pty.send(PtyCommand::Resize(PtySize::new(size.cols, size.rows)));
            }
            Incoming::Command(SessionCommand::Scroll(lines)) => {
                engine.scroll(lines);
                if let Ok(snapshot) = engine.snapshot() {
                    let _ = events.send(SessionEvent::Snapshot(snapshot));
                }
            }
            Incoming::Command(SessionCommand::Search { query, direction }) => {
                match engine.search(&query, direction) {
                    Ok(result) => {
                        let _ = events.send(SessionEvent::SearchResult(result));
                        if let Ok(snapshot) = engine.snapshot() {
                            let _ = events.send(SessionEvent::Snapshot(snapshot));
                        }
                    }
                    Err(error) => {
                        let _ = events.send(SessionEvent::Error(error.to_string()));
                    }
                }
            }
            Incoming::Command(SessionCommand::ClearScrollback) => {
                engine.clear_scrollback();
                if let Ok(snapshot) = engine.snapshot() {
                    let _ = events.send(SessionEvent::Snapshot(snapshot));
                }
            }
            Incoming::Command(SessionCommand::ExportHistory(sender)) => {
                let _ = sender.send(engine.select_all_text().unwrap_or_default());
            }
            Incoming::Command(SessionCommand::RestoreHistory(text)) => {
                engine.feed(&normalize_restored_history(&text));
                let _ = engine.take_effects();
                if let Ok(snapshot) = engine.snapshot() {
                    let _ = events.send(SessionEvent::Snapshot(snapshot));
                }
            }
            Incoming::Command(SessionCommand::SetTheme(theme)) => {
                match engine.apply_theme(&theme).and_then(|_| engine.snapshot()) {
                    Ok(snapshot) => {
                        let _ = events.send(SessionEvent::Snapshot(snapshot));
                    }
                    Err(error) => {
                        let _ = events.send(SessionEvent::Error(error.to_string()));
                    }
                }
            }
            Incoming::Command(SessionCommand::Shutdown) => return,
            Incoming::Pty(PtyEvent::Output(bytes)) => {
                process_pty_output_chunk(
                    &bytes,
                    &mut engine,
                    &mut osc,
                    &pty,
                    &events,
                    &mut reply_limiter,
                    &mut reply_limit_warned,
                );
                for _ in 0..MAX_PTY_OUTPUT_CHUNKS_PER_BATCH {
                    let Ok(event) = pty.events().try_recv() else {
                        break;
                    };
                    match event {
                        PtyEvent::Output(bytes) => {
                            process_pty_output_chunk(
                                &bytes,
                                &mut engine,
                                &mut osc,
                                &pty,
                                &events,
                                &mut reply_limiter,
                                &mut reply_limit_warned,
                            );
                        }
                        other => {
                            pending_pty = Some(other);
                            break;
                        }
                    }
                }
                match engine.snapshot() {
                    Ok(snapshot) => {
                        let _ = events.send(SessionEvent::Snapshot(snapshot));
                    }
                    Err(error) => {
                        let _ = events.send(SessionEvent::Error(error.to_string()));
                    }
                }
            }
            Incoming::Pty(PtyEvent::Exited(status)) => {
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
    fn resize_updates_snapshot_dimensions() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        terminal
            .resize(TerminalSize::new(20, 4).unwrap(), 8, 16)
            .unwrap();
        let snapshot = terminal.snapshot().unwrap();
        assert_eq!((snapshot.cols, snapshot.rows), (20, 4));
    }

    #[test]
    fn key_encoder_tracks_terminal_application_cursor_mode() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        let input = KeyInput {
            key: InputKey::Up,
            text: None,
            modifiers: KeyModifiers::default(),
            repeat: false,
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
        };
        assert_eq!(terminal.encode_key(&input).unwrap(), b" ");
    }

    #[test]
    fn control_keys_ignore_platform_control_text() {
        let mut terminal = TerminalEngine::new(TerminalSize::new(10, 2).unwrap()).unwrap();
        // Codex and similar TUIs may enable Kitty keyboard disambiguation.
        terminal.feed(b"\x1b[>1u");
        for (key, text, expected) in [
            (InputKey::Character('c'), "\u{3}", b"\x03".as_slice()),
            (InputKey::Character('d'), "\u{4}", b"\x04".as_slice()),
        ] {
            let input = KeyInput {
                key,
                text: Some(text.into()),
                modifiers: KeyModifiers {
                    control: true,
                    ..KeyModifiers::default()
                },
                repeat: false,
            };
            assert_eq!(terminal.encode_key(&input).unwrap(), expected);
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
        let text = terminal.snapshot().unwrap().to_plain_text();
        assert!(text.contains("中文"));
        assert!(text.contains('🙂'));
        assert!(text.contains("e\u{301}"));
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
