use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub const DEFAULT_SCROLLBACK_BYTES: usize = 128_000_000;
pub const DEFAULT_SCROLLBACK_LINES: usize = 100_000;
const LEGACY_DEFAULT_SCROLLBACK_BYTES: usize = 100_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub navigation: NavigationConfig,
    pub terminal: TerminalConfig,
    pub window: WindowConfig,
    pub sidebar: SidebarConfig,
    pub workspace: WorkspaceConfig,
    pub agent: AgentConfig,
    pub theme: ThemeConfig,
    pub keybindings: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiConfig::default(),
            navigation: NavigationConfig::default(),
            terminal: TerminalConfig::default(),
            window: WindowConfig::default(),
            sidebar: SidebarConfig::default(),
            workspace: WorkspaceConfig::default(),
            agent: AgentConfig::default(),
            theme: ThemeConfig::default(),
            keybindings: HashMap::from([
                ("ctrl+shift+p".into(), "command_palette".into()),
                ("ctrl+shift+d".into(), "split:right".into()),
                ("ctrl+shift+e".into(), "split:down".into()),
                ("ctrl+,".into(), "settings.open".into()),
                ("ctrl+shift+f".into(), "terminal.find_directory".into()),
            ]),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NavigationPosition {
    Top,
    #[default]
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NavigationItem {
    Sidebar,
    NewWorkspace,
    CommandPalette,
    Notifications,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct NavigationConfig {
    pub visible: bool,
    pub position: NavigationPosition,
    pub items: Vec<NavigationItem>,
}

impl Default for NavigationConfig {
    fn default() -> Self {
        Self {
            visible: true,
            position: NavigationPosition::Bottom,
            items: vec![NavigationItem::Sidebar, NavigationItem::Notifications],
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiLanguage {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-CN", alias = "zh-cn", alias = "zh")]
    SimplifiedChinese,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// `auto` follows the operating-system locale; supported explicit values are
    /// `en` and `zh-CN`.
    pub language: UiLanguage,
    /// Keep wmcterm's built-in shortcuts in addition to entries in `[keybindings]`.
    /// Disable this when a terminal/TUI should receive every unconfigured chord.
    pub legacy_shortcuts: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: UiLanguage::Auto,
            legacy_shortcuts: true,
        }
    }
}

impl Config {
    /// Parse and validate configured shortcuts, resolving legacy action names.
    pub fn resolved_keybindings(&self) -> Result<ResolvedKeybindings, KeybindingErrors> {
        ResolvedKeybindings::parse(&self.keybindings)
    }

    /// Apply the subset of a Ghostty configuration that wmcterm understands.
    pub fn apply_ghostty(&mut self, imported: &GhosttyImport) {
        if let Some(value) = &imported.font_family {
            self.terminal.font_family.clone_from(value);
        }
        if let Some(value) = imported.font_size {
            self.terminal.font_size = value;
        }
        if let Some(value) = imported.scrollback {
            self.terminal.scrollback = value;
        }
        if let Some(value) = imported.opacity {
            self.window.opacity = value;
        }
        imported.theme.apply_to(&mut self.theme);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct TerminalConfig {
    pub font_family: String,
    pub font_size: f32,
    /// Maximum primary-screen scrollback allocation in bytes. This matches
    /// Ghostty's `scrollback-limit` unit.
    pub scrollback: usize,
    /// Maximum logical history rows exposed by scrolling, search, selection,
    /// and session persistence.
    pub scrollback_lines: usize,
    pub bell_notifications: BellNotificationMode,
    pub bell_debounce_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BellNotificationMode {
    Never,
    #[default]
    Background,
    Always,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            font_family: "Maple Mono Normal NF".into(),
            font_size: 14.0,
            scrollback: DEFAULT_SCROLLBACK_BYTES,
            scrollback_lines: DEFAULT_SCROLLBACK_LINES,
            bell_notifications: BellNotificationMode::Background,
            bell_debounce_ms: 1_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    pub decorations: bool,
    pub opacity: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            decorations: true,
            opacity: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarConfig {
    pub position: String,
    pub width: u16,
}

impl Default for SidebarConfig {
    fn default() -> Self {
        Self {
            position: "left".into(),
            width: 260,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct WorkspaceConfig {
    pub inherit_cwd: bool,
    pub default_cwd: Option<PathBuf>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            inherit_cwd: true,
            default_cwd: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub notifications: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            notifications: true,
        }
    }
}

/// An sRGB color. TOML uses `#rrggbb` or `#rrggbbaa`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThemeColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl ThemeColor {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

impl FromStr for ThemeColor {
    type Err = String;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let value = input.trim().trim_start_matches('#');
        if value.len() != 6 && value.len() != 8 {
            return Err("expected #rrggbb or #rrggbbaa".into());
        }
        let byte = |offset| {
            u8::from_str_radix(&value[offset..offset + 2], 16)
                .map_err(|_| "color contains a non-hexadecimal digit".to_owned())
        };
        Ok(Self {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if value.len() == 8 { byte(6)? } else { 255 },
        })
    }
}

impl fmt::Display for ThemeColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.r, self.g, self.b, self.a
            )
        }
    }
}

impl Serialize for ThemeColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ThemeConfig {
    pub background: ThemeColor,
    pub foreground: ThemeColor,
    pub cursor: ThemeColor,
    pub cursor_text: ThemeColor,
    pub selection_background: ThemeColor,
    pub selection_foreground: ThemeColor,
    pub sidebar_background: ThemeColor,
    pub panel_background: ThemeColor,
    pub hover_background: ThemeColor,
    pub border: ThemeColor,
    pub active_border: ThemeColor,
    pub muted_foreground: ThemeColor,
    pub weak_foreground: ThemeColor,
    pub success: ThemeColor,
    pub warning: ThemeColor,
    pub error: ThemeColor,
    /// ANSI colors 0 through 15. Invalid-length user palettes are rejected by `validate`.
    pub palette: Vec<ThemeColor>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            background: ThemeColor::rgb(28, 28, 28),
            foreground: ThemeColor::rgb(228, 228, 228),
            cursor: ThemeColor::rgb(122, 162, 214),
            cursor_text: ThemeColor::rgb(28, 28, 28),
            selection_background: ThemeColor::rgb(48, 48, 48),
            selection_foreground: ThemeColor::rgb(228, 228, 228),
            sidebar_background: ThemeColor::rgb(24, 24, 24),
            panel_background: ThemeColor::rgb(36, 36, 36),
            hover_background: ThemeColor::rgb(41, 41, 41),
            border: ThemeColor::rgb(51, 51, 51),
            active_border: ThemeColor::rgb(122, 162, 214),
            muted_foreground: ThemeColor::rgb(160, 160, 160),
            weak_foreground: ThemeColor::rgb(111, 111, 111),
            success: ThemeColor::rgb(129, 178, 154),
            warning: ThemeColor::rgb(208, 173, 120),
            error: ThemeColor::rgb(201, 123, 123),
            palette: [
                "#181818", "#c97b7b", "#81b29a", "#d0ad78", "#7aa2d6", "#a98cb3", "#7fa9a5",
                "#c8c8c8", "#6f6f6f", "#d88c8c", "#91c2aa", "#debd88", "#8cb4e6", "#ba9dc4",
                "#90bab5", "#e4e4e4",
            ]
            .map(|value| value.parse().expect("static theme color is valid"))
            .into(),
        }
    }
}

impl ThemeConfig {
    pub fn validate(&self) -> Result<(), ThemeError> {
        if self.palette.len() == 16 {
            Ok(())
        } else {
            Err(ThemeError::PaletteLength(self.palette.len()))
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ThemeError {
    #[error("theme palette must contain exactly 16 colors, found {0}")]
    PaletteLength(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Shortcut {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_key: bool,
    pub key: String,
}

impl FromStr for Shortcut {
    type Err = ShortcutError;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut shortcut = Self {
            ctrl: false,
            alt: false,
            shift: false,
            super_key: false,
            key: String::new(),
        };
        let mut seen = HashSet::new();
        for raw in input.split('+') {
            let token = raw.trim().to_ascii_lowercase();
            if token.is_empty() {
                return Err(ShortcutError::EmptyPart);
            }
            let canonical = match token.as_str() {
                "control" => "ctrl",
                "option" => "alt",
                "cmd" | "command" | "meta" | "win" => "super",
                value => value,
            };
            if !seen.insert(canonical.to_owned()) {
                return Err(ShortcutError::DuplicatePart(canonical.to_owned()));
            }
            match canonical {
                "ctrl" => shortcut.ctrl = true,
                "alt" => shortcut.alt = true,
                "shift" => shortcut.shift = true,
                "super" => shortcut.super_key = true,
                key if shortcut.key.is_empty() && valid_key(key) => {
                    shortcut.key = normalize_key(key)
                }
                key if !shortcut.key.is_empty() => {
                    return Err(ShortcutError::MultipleKeys(shortcut.key, key.to_owned()));
                }
                key => return Err(ShortcutError::UnknownKey(key.to_owned())),
            }
        }
        if shortcut.key.is_empty() {
            Err(ShortcutError::MissingKey)
        } else {
            Ok(shortcut)
        }
    }
}

fn valid_key(key: &str) -> bool {
    key.chars().count() == 1
        || matches!(
            key,
            "enter"
                | "return"
                | "tab"
                | "space"
                | "escape"
                | "esc"
                | "backspace"
                | "delete"
                | "insert"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
                | "up"
                | "down"
                | "left"
                | "right"
                | "minus"
                | "equal"
                | "comma"
                | "period"
                | "slash"
                | "backslash"
                | "semicolon"
                | "quote"
                | "bracketleft"
                | "bracketright"
                | "backtick"
        )
        || key
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n))
}

fn normalize_key(key: &str) -> String {
    match key {
        "return" => "enter",
        "esc" => "escape",
        value => value,
    }
    .to_owned()
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (enabled, name) in [
            (self.ctrl, "ctrl"),
            (self.alt, "alt"),
            (self.shift, "shift"),
            (self.super_key, "super"),
        ] {
            if enabled {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(&self.key)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ShortcutError {
    #[error("shortcut contains an empty component")]
    EmptyPart,
    #[error("shortcut repeats component `{0}`")]
    DuplicatePart(String),
    #[error("shortcut must contain one non-modifier key")]
    MissingKey,
    #[error("shortcut contains multiple keys: `{0}` and `{1}`")]
    MultipleKeys(String, String),
    #[error("unknown key name `{0}`")]
    UnknownKey(String),
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedKeybindings(BTreeMap<Shortcut, String>);

impl ResolvedKeybindings {
    pub fn parse(raw: &HashMap<String, String>) -> Result<Self, KeybindingErrors> {
        let mut bindings = BTreeMap::new();
        let mut errors = Vec::new();
        let mut entries: Vec<_> = raw.iter().collect();
        entries.sort_by_key(|(shortcut, _)| *shortcut);
        for (source, action) in entries {
            let shortcut = match source.parse::<Shortcut>() {
                Ok(value) => value,
                Err(error) => {
                    errors.push(format!("{source}: {error}"));
                    continue;
                }
            };
            let Some(action) = canonical_action(action) else {
                errors.push(format!("{source}: unknown action `{action}`"));
                continue;
            };
            if let Some(previous) = bindings.insert(shortcut.clone(), action.to_owned()) {
                errors.push(format!("duplicate normalized shortcut `{shortcut}` (actions `{previous}` and `{action}`)"));
            }
        }
        if errors.is_empty() {
            Ok(Self(bindings))
        } else {
            Err(KeybindingErrors(errors))
        }
    }
    pub fn action_for(&self, shortcut: &Shortcut) -> Option<&str> {
        self.0.get(shortcut).map(String::as_str)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&Shortcut, &str)> {
        self.0.iter().map(|(key, value)| (key, value.as_str()))
    }
}

fn canonical_action(action: &str) -> Option<&'static str> {
    Some(match action.trim().to_ascii_lowercase().as_str() {
        "command_palette" | "command.palette" => "command.palette",
        "split:right" | "pane.split_right" => "pane.split_right",
        "split:down" | "pane.split_down" => "pane.split_down",
        "workspace.new" => "workspace.new",
        "workspace.close" => "workspace.close",
        "workspace.rename" => "workspace.rename",
        "workspace.change_directory" => "workspace.change_directory",
        "workspace.describe" => "workspace.describe",
        "surface.new" => "surface.new",
        "surface.close" => "surface.close",
        "agent.claude" => "agent.claude",
        "agent.codex" => "agent.codex",
        "agent.gemini" => "agent.gemini",
        "agent.opencode" => "agent.opencode",
        "pane.close" => "pane.close",
        "pane.zoom" => "pane.zoom",
        "terminal.find" => "terminal.find",
        "terminal.find_directory" => "terminal.find_directory",
        "terminal.clear_scrollback" => "terminal.clear_scrollback",
        "terminal.font_increase" => "terminal.font_increase",
        "terminal.font_decrease" => "terminal.font_decrease",
        "terminal.font_reset" => "terminal.font_reset",
        "sidebar.toggle" => "sidebar.toggle",
        "notifications.toggle" => "notifications.toggle",
        "notifications.latest_unread" => "notifications.latest_unread",
        "notifications.toggle_unread" => "notifications.toggle_unread",
        "window.new" => "window.new",
        "settings.open" => "settings.open",
        "app.quit" => "app.quit",
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingErrors(pub Vec<String>);
impl fmt::Display for KeybindingErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("; "))
    }
}
impl std::error::Error for KeybindingErrors {}

#[derive(Debug, Clone, Default)]
pub struct GhosttyImport {
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub scrollback: Option<usize>,
    pub opacity: Option<f32>,
    pub theme_name: Option<String>,
    pub theme: GhosttyTheme,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GhosttyTheme {
    pub background: Option<ThemeColor>,
    pub foreground: Option<ThemeColor>,
    pub cursor: Option<ThemeColor>,
    pub cursor_text: Option<ThemeColor>,
    pub selection_background: Option<ThemeColor>,
    pub selection_foreground: Option<ThemeColor>,
    pub palette: BTreeMap<usize, ThemeColor>,
}

impl GhosttyTheme {
    fn apply_to(&self, target: &mut ThemeConfig) {
        if let Some(v) = self.background {
            target.background = v;
        }
        if let Some(v) = self.foreground {
            target.foreground = v;
        }
        if let Some(v) = self.cursor {
            target.cursor = v;
        }
        if let Some(v) = self.cursor_text {
            target.cursor_text = v;
        }
        if let Some(v) = self.selection_background {
            target.selection_background = v;
        }
        if let Some(v) = self.selection_foreground {
            target.selection_foreground = v;
        }
        for (&index, &color) in &self.palette {
            if index < target.palette.len() {
                target.palette[index] = color;
            }
        }
    }

    fn overlay(&mut self, newer: GhosttyTheme) {
        if newer.background.is_some() {
            self.background = newer.background;
        }
        if newer.foreground.is_some() {
            self.foreground = newer.foreground;
        }
        if newer.cursor.is_some() {
            self.cursor = newer.cursor;
        }
        if newer.cursor_text.is_some() {
            self.cursor_text = newer.cursor_text;
        }
        if newer.selection_background.is_some() {
            self.selection_background = newer.selection_background;
        }
        if newer.selection_foreground.is_some() {
            self.selection_foreground = newer.selection_foreground;
        }
        self.palette.extend(newer.palette);
    }
}

pub fn parse_ghostty(source: &str) -> GhosttyImport {
    let mut result = GhosttyImport::default();
    for (line_number, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            result.warnings.push(format!(
                "line {}: ignored malformed setting",
                line_number + 1
            ));
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let warn = |result: &mut GhosttyImport, expected: &str| {
            result.warnings.push(format!(
                "line {}: invalid {key}, expected {expected}",
                line_number + 1
            ))
        };
        match key {
            "font-family" if !value.is_empty() => result.font_family = Some(value.to_owned()),
            "font-size" => match value.parse::<f32>() {
                Ok(v) if v.is_finite() && v > 0.0 => result.font_size = Some(v),
                _ => warn(&mut result, "a positive number"),
            },
            "scrollback-limit" => match value.parse::<usize>() {
                Ok(v) => result.scrollback = Some(v),
                _ => warn(&mut result, "a non-negative integer"),
            },
            "background-opacity" => match value.parse::<f32>() {
                Ok(v) if (0.0..=1.0).contains(&v) => result.opacity = Some(v),
                _ => warn(&mut result, "a number from 0 to 1"),
            },
            "theme" if !value.is_empty() => result.theme_name = Some(value.to_owned()),
            "background" => parse_theme_color(value, &mut result, line_number, |t, v| {
                t.background = Some(v)
            }),
            "foreground" => parse_theme_color(value, &mut result, line_number, |t, v| {
                t.foreground = Some(v)
            }),
            "cursor-color" => {
                parse_theme_color(value, &mut result, line_number, |t, v| t.cursor = Some(v))
            }
            "cursor-text" => parse_theme_color(value, &mut result, line_number, |t, v| {
                t.cursor_text = Some(v)
            }),
            "selection-background" => parse_theme_color(value, &mut result, line_number, |t, v| {
                t.selection_background = Some(v)
            }),
            "selection-foreground" => parse_theme_color(value, &mut result, line_number, |t, v| {
                t.selection_foreground = Some(v)
            }),
            "palette" => {
                if let Some((index, color)) = value.split_once('=') {
                    match (
                        index.trim().parse::<usize>(),
                        color.trim().parse::<ThemeColor>(),
                    ) {
                        (Ok(index @ 0..=255), Ok(color)) => {
                            result.theme.palette.insert(index, color);
                        }
                        _ => warn(&mut result, "INDEX=#rrggbb"),
                    }
                } else {
                    warn(&mut result, "INDEX=#rrggbb");
                }
            }
            _ => {}
        }
    }
    result
}

fn parse_theme_color(
    value: &str,
    result: &mut GhosttyImport,
    line: usize,
    set: impl FnOnce(&mut GhosttyTheme, ThemeColor),
) {
    match value.parse() {
        Ok(value) => set(&mut result.theme, value),
        Err(error) => result
            .warnings
            .push(format!("line {}: invalid color: {error}", line + 1)),
    }
}

/// Read a Ghostty config and, if requested, merge a named theme from the supplied directories.
pub fn load_ghostty(path: &Path, theme_dirs: &[PathBuf]) -> Result<GhosttyImport, ConfigError> {
    let source = read_text(path)?;
    let mut imported = parse_ghostty(&source);
    if let Some(name) = imported.theme_name.clone() {
        let theme_path = Path::new(&name);
        let found = if theme_path.is_absolute() && theme_path.is_file() {
            Some(theme_path.to_owned())
        } else if !name.contains('/') && !name.contains('\\') {
            theme_dirs
                .iter()
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        } else {
            None
        };
        if let Some(path) = found {
            let theme = parse_ghostty(&read_text(&path)?);
            let explicit = std::mem::take(&mut imported.theme);
            imported.theme = theme.theme;
            imported.theme.overlay(explicit);
            imported.warnings.extend(theme.warnings);
        } else {
            imported
                .warnings
                .push(format!("Ghostty theme `{name}` was not found"));
        }
    }
    Ok(imported)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub workspace: ProjectWorkspace,
    #[serde(rename = "command")]
    pub commands: Vec<ProjectCommand>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectWorkspace {
    pub name: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCommand {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error(
        "workspace default directory `{directory}` from {path} does not exist or is not a directory"
    )]
    InvalidWorkspaceDirectory { path: PathBuf, directory: PathBuf },
}

pub fn config_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "wmcterm", "wmcterm").map(|dirs| dirs.config_dir().join("config.toml"))
}
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let mut config: Config = parse_file(path)?;
    migrate_legacy_scrollback_default(&mut config);
    validate_workspace_directory(&mut config, path)?;
    Ok(config)
}
pub fn load_or_default() -> Result<Config, ConfigError> {
    match config_path() {
        Some(path) if path.exists() => load(&path),
        _ => Ok(Config::default()),
    }
}

/// Load TOML layers in order. Tables merge recursively and later scalar/array values win.
pub fn load_layered(paths: &[PathBuf]) -> Result<Config, ConfigError> {
    let mut merged = toml::Value::try_from(Config::default()).expect("default config serializes");
    for path in paths {
        if !path.exists() {
            continue;
        }
        let overlay: toml::Value = parse_file(path)?;
        merge_values(&mut merged, overlay);
    }
    let mut config: Config = merged.try_into().map_err(|source| ConfigError::Parse {
        path: paths.last().cloned().unwrap_or_default(),
        source,
    })?;
    migrate_legacy_scrollback_default(&mut config);
    validate_workspace_directory(
        &mut config,
        paths
            .last()
            .map(PathBuf::as_path)
            .unwrap_or_else(|| Path::new("<defaults>")),
    )?;
    Ok(config)
}

fn migrate_legacy_scrollback_default(config: &mut Config) {
    if config.terminal.scrollback == LEGACY_DEFAULT_SCROLLBACK_BYTES {
        config.terminal.scrollback = DEFAULT_SCROLLBACK_BYTES;
    }
}

fn validate_workspace_directory(config: &mut Config, source: &Path) -> Result<(), ConfigError> {
    let Some(configured) = config.workspace.default_cwd.as_ref() else {
        return Ok(());
    };
    let expanded = expand_home_path(configured);
    let resolved = expanded
        .canonicalize()
        .map_err(|_| ConfigError::InvalidWorkspaceDirectory {
            path: source.to_path_buf(),
            directory: expanded.clone(),
        })?;
    if !resolved.is_dir() {
        return Err(ConfigError::InvalidWorkspaceDirectory {
            path: source.to_path_buf(),
            directory: resolved,
        });
    }
    config.workspace.default_cwd = Some(normalize_windows_path(resolved));
    Ok(())
}

/// Converts Windows verbatim paths returned by `canonicalize` back to the
/// conventional form used in configuration, session state, and the UI.
pub fn normalize_windows_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        let verbatim_unc = r"\\?\UNC\".encode_utf16().collect::<Vec<_>>();
        let verbatim = r"\\?\".encode_utf16().collect::<Vec<_>>();
        if let Some(rest) = wide.strip_prefix(verbatim_unc.as_slice()) {
            let mut conventional = r"\\".encode_utf16().collect::<Vec<_>>();
            conventional.extend_from_slice(rest);
            return PathBuf::from(OsString::from_wide(&conventional));
        }
        if let Some(rest) = wide.strip_prefix(verbatim.as_slice()) {
            return PathBuf::from(OsString::from_wide(rest));
        }
    }
    path
}

/// Expands a leading `~` path component using the platform home directory.
pub fn expand_home_path(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    let Some(home) = BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return path.to_path_buf();
    };
    if rest.as_os_str().is_empty() {
        home
    } else {
        home.join(rest)
    }
}

pub fn merge_values(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = base.get_mut(&key) {
                    merge_values(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Small polling-friendly reloader. It performs no background IO and never writes files.
pub struct ConfigReloader {
    paths: Vec<PathBuf>,
    stamps: Vec<Option<(SystemTime, u64)>>,
}
impl ConfigReloader {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let stamps = paths.iter().map(|path| file_stamp(path)).collect();
        Self { paths, stamps }
    }
    pub fn reload(&mut self) -> Result<Config, ConfigError> {
        let config = load_layered(&self.paths)?;
        self.stamps = self.paths.iter().map(|path| file_stamp(path)).collect();
        Ok(config)
    }
    pub fn reload_if_changed(&mut self) -> Result<Option<Config>, ConfigError> {
        let current: Vec<_> = self.paths.iter().map(|path| file_stamp(path)).collect();
        if current == self.stamps {
            Ok(None)
        } else {
            self.reload().map(Some)
        }
    }
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}
fn file_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let metadata = path.metadata().ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

pub fn load_project(root: &Path) -> Result<Option<ProjectConfig>, ConfigError> {
    let path = root.join(".wmcterm.toml");
    if !path.exists() {
        return Ok(None);
    }
    parse_file(&path).map(Some)
}

fn read_text(path: &Path) -> Result<String, ConfigError> {
    fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.into(),
        source,
    })
}
fn parse_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ConfigError> {
    let source = read_text(path)?;
    toml::from_str(&source).map_err(|source| ConfigError::Parse {
        path: path.into(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_ui_languages_and_defaults_to_auto() {
        let default: Config = toml::from_str("").unwrap();
        assert_eq!(default.ui.language, UiLanguage::Auto);

        let chinese: Config = toml::from_str("[ui]\nlanguage = \"zh-CN\"\n").unwrap();
        assert_eq!(chinese.ui.language, UiLanguage::SimplifiedChinese);

        let english: Config = toml::from_str("[ui]\nlanguage = \"en\"\n").unwrap();
        assert_eq!(english.ui.language, UiLanguage::English);

        assert!(toml::from_str::<Config>("[ui]\nlanguage = \"fr\"\n").is_err());
    }

    #[test]
    fn parses_documented_configuration_and_keeps_defaults() {
        let value: Config = toml::from_str("[terminal]\nfont-family='Maple Mono'\nfont-size=15\nscrollback=42\n[sidebar]\nwidth=300").unwrap();
        assert_eq!(value.terminal.font_family, "Maple Mono");
        assert_eq!(value.sidebar.width, 300);
        assert!(value.agent.notifications);
        assert!(value.navigation.visible);
        assert_eq!(value.navigation.position, NavigationPosition::Bottom);
        assert_eq!(
            Config::default().terminal.font_family,
            "Maple Mono Normal NF"
        );
        assert_eq!(Config::default().terminal.scrollback, 128_000_000);
        assert_eq!(Config::default().terminal.scrollback_lines, 100_000);
        assert_eq!(value.theme.background.to_string(), "#1c1c1c");
        assert_eq!(value.theme.sidebar_background.to_string(), "#181818");
        assert_eq!(value.theme.panel_background.to_string(), "#242424");
        assert_eq!(value.theme.hover_background.to_string(), "#292929");
        assert_eq!(value.theme.selection_background.to_string(), "#303030");
        assert_eq!(value.theme.border.to_string(), "#333333");
        assert_eq!(value.theme.active_border.to_string(), "#7aa2d6");
        assert_eq!(value.theme.success.to_string(), "#81b29a");
        assert_eq!(value.theme.warning.to_string(), "#d0ad78");
        assert_eq!(value.theme.error.to_string(), "#c97b7b");
        assert_eq!(
            value.terminal.bell_notifications,
            BellNotificationMode::Background
        );
        let bells: Config =
            toml::from_str("[terminal]\nbell-notifications='never'\nbell-debounce-ms=2500")
                .unwrap();
        assert_eq!(
            bells.terminal.bell_notifications,
            BellNotificationMode::Never
        );
        assert_eq!(bells.terminal.bell_debounce_ms, 2_500);
    }

    #[test]
    fn migrates_the_old_too_small_scrollback_default() {
        let mut config = Config::default();
        config.terminal.scrollback = LEGACY_DEFAULT_SCROLLBACK_BYTES;
        migrate_legacy_scrollback_default(&mut config);
        assert_eq!(config.terminal.scrollback, DEFAULT_SCROLLBACK_BYTES);
    }

    #[test]
    fn parses_custom_navigation_bar() {
        let value: Config = toml::from_str(
            "[navigation]\nvisible=true\nposition='top'\nitems=['notifications','sidebar']",
        )
        .unwrap();
        assert_eq!(value.navigation.position, NavigationPosition::Top);
        assert_eq!(
            value.navigation.items,
            vec![NavigationItem::Notifications, NavigationItem::Sidebar]
        );
    }

    #[test]
    fn workspace_default_directory_is_optional_and_kebab_case() {
        let defaults: Config = toml::from_str("").unwrap();
        assert_eq!(defaults.workspace.default_cwd, None);

        let configured: Config =
            toml::from_str("[workspace]\ndefault-cwd='/tmp'\ninherit-cwd=false").unwrap();
        assert_eq!(
            configured.workspace.default_cwd,
            Some(PathBuf::from("/tmp"))
        );
        assert!(!configured.workspace.inherit_cwd);
    }

    #[test]
    fn loading_rejects_a_workspace_default_that_is_not_a_directory() {
        let path = std::env::temp_dir().join(format!(
            "wmcterm-config-missing-{}-{}.toml",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            "[workspace]\ndefault-cwd='/definitely/missing/wmcterm-directory'\n",
        )
        .unwrap();
        let error = load(&path).unwrap_err().to_string();
        std::fs::remove_file(path).unwrap();
        assert!(error.contains("does not exist or is not a directory"));
    }

    #[test]
    fn shortcut_normalizes_aliases_and_resolves_legacy_actions() {
        let shortcut: Shortcut = "Shift+Control+Return".parse().unwrap();
        assert_eq!(shortcut.to_string(), "ctrl+shift+enter");
        let bindings = Config::default().resolved_keybindings().unwrap();
        assert_eq!(
            bindings.action_for(&"shift+ctrl+p".parse().unwrap()),
            Some("command.palette")
        );
    }

    #[test]
    fn shortcut_validation_reports_duplicate_and_unknown_entries() {
        let raw = HashMap::from([
            ("ctrl+shift+p".to_owned(), "app.quit".to_owned()),
            ("Shift+Control+p".to_owned(), "terminal.find".to_owned()),
            ("ctrl+mystery".to_owned(), "no.such.action".to_owned()),
        ]);
        let error = ResolvedKeybindings::parse(&raw).unwrap_err().to_string();
        assert!(error.contains("duplicate normalized shortcut"));
        assert!(error.contains("unknown key name `mystery`"));
        assert_eq!(
            "ctrl+ctrl+p".parse::<Shortcut>().unwrap_err(),
            ShortcutError::DuplicatePart("ctrl".into())
        );

        let unknown_action = HashMap::from([("ctrl+x".to_owned(), "no.such.action".to_owned())]);
        assert!(
            ResolvedKeybindings::parse(&unknown_action)
                .unwrap_err()
                .to_string()
                .contains("unknown action `no.such.action`")
        );
    }

    #[test]
    fn theme_color_round_trips_and_palette_is_validated() {
        let color: ThemeColor = "#89B4FA80".parse().unwrap();
        assert_eq!(color.to_string(), "#89b4fa80");
        let mut theme = ThemeConfig::default();
        assert_eq!(theme.palette.len(), 16);
        theme.palette.pop();
        assert_eq!(theme.validate(), Err(ThemeError::PaletteLength(15)));
    }

    #[test]
    fn imports_ghostty_settings_without_failing_on_unknown_options() {
        let imported = parse_ghostty(
            "font-family = JetBrains Mono\nfont-size = 15.5\nbackground = 11111b\npalette = 1=#ff0000\nwindow-save-state = always\nbackground-opacity = nope\n",
        );
        assert_eq!(imported.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(imported.theme.background, Some(ThemeColor::rgb(17, 17, 27)));
        assert_eq!(imported.theme.palette[&1], ThemeColor::rgb(255, 0, 0));
        assert_eq!(imported.warnings.len(), 1);
        let mut config = Config::default();
        config.apply_ghostty(&imported);
        assert_eq!(config.terminal.font_size, 15.5);
    }

    #[test]
    fn explicit_ghostty_colors_override_named_theme_colors() {
        let mut named = GhosttyTheme {
            background: Some(ThemeColor::rgb(1, 2, 3)),
            foreground: Some(ThemeColor::rgb(4, 5, 6)),
            ..GhosttyTheme::default()
        };
        named.overlay(GhosttyTheme {
            background: Some(ThemeColor::rgb(10, 20, 30)),
            ..GhosttyTheme::default()
        });
        assert_eq!(named.background, Some(ThemeColor::rgb(10, 20, 30)));
        assert_eq!(named.foreground, Some(ThemeColor::rgb(4, 5, 6)));
    }

    #[test]
    fn recursive_merge_preserves_unmentioned_defaults() {
        let mut base: toml::Value =
            toml::from_str("[terminal]\nfont-family='Maple'\nfont-size=14\n[sidebar]\nwidth=260")
                .unwrap();
        let overlay = toml::from_str("[terminal]\nfont-size=18").unwrap();
        merge_values(&mut base, overlay);
        assert_eq!(base["terminal"]["font-family"].as_str(), Some("Maple"));
        assert_eq!(base["terminal"]["font-size"].as_integer(), Some(18));
        assert_eq!(base["sidebar"]["width"].as_integer(), Some(260));
    }
}
