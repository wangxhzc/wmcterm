//! Non-terminal surface backends.
//!
//! The types in this crate deliberately contain no window-system code.  They expose a
//! small, object-safe contract which the application can poll and render in whichever
//! UI toolkit it uses.

use std::any::Any;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use thiserror::Error;
use uuid::Uuid;

pub const MAX_LOG_LINE_BYTES: usize = 64 * 1024;
pub const MAX_LOG_HISTORY_BYTES: usize = 8 * 1024 * 1024;
pub const LOG_READ_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Browser,
    Markdown,
    Log,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// One-based source line number.
    pub line: usize,
    pub text: String,
}

pub trait SurfaceBackend: Send {
    fn id(&self) -> Uuid;
    fn title(&self) -> &str;
    fn render_text(&self) -> String;

    fn kind(&self) -> SurfaceKind;

    /// Typed access for UI layers that can provide richer presentation than the
    /// portable plain-text fallback.
    fn as_any(&self) -> &dyn Any;

    fn as_any_mut(&mut self) -> &mut dyn Any;

    /// Poll the backend for externally changed content.
    fn refresh(&mut self) -> Result<bool, SurfaceError> {
        Ok(false)
    }

    /// Case-insensitive text search. Implementations may override this to retain
    /// original source line numbers after filtering.
    fn search(&self, query: &str) -> Vec<SearchMatch> {
        search_lines(&self.render_text(), query)
    }
}

#[derive(Debug, Clone)]
pub struct BrowserSurface {
    id: Uuid,
    title: String,
    pub url: String,
}

impl BrowserSurface {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            id: Uuid::new_v4(),
            title: url.clone(),
            url,
        }
    }

    pub fn navigate(&mut self, url: impl Into<String>) {
        self.url = url.into();
        self.title.clone_from(&self.url);
    }
}

impl SurfaceBackend for BrowserSurface {
    fn id(&self) -> Uuid {
        self.id
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn render_text(&self) -> String {
        format!(
            "Browser\n\n{}\n\nWebView integration is platform-provided.",
            self.url
        )
    }

    fn kind(&self) -> SurfaceKind {
        SurfaceKind::Browser
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    List {
        ordered: bool,
        items: Vec<String>,
    },
    Quote(String),
    ThematicBreak,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MarkdownDocument {
    pub blocks: Vec<MarkdownBlock>,
}

impl MarkdownDocument {
    pub fn parse(source: &str) -> Self {
        let lines: Vec<&str> = source.lines().collect();
        let mut blocks = Vec::new();
        let mut index = 0;

        while index < lines.len() {
            let line = lines[index];
            if line.trim().is_empty() {
                index += 1;
                continue;
            }

            if let Some(fence) = line.trim_start().strip_prefix("```") {
                let language = (!fence.trim().is_empty()).then(|| fence.trim().to_owned());
                index += 1;
                let mut code = Vec::new();
                while index < lines.len() && !lines[index].trim_start().starts_with("```") {
                    code.push(lines[index]);
                    index += 1;
                }
                if index < lines.len() {
                    index += 1;
                }
                blocks.push(MarkdownBlock::CodeBlock {
                    language,
                    code: code.join("\n"),
                });
                continue;
            }

            if let Some((level, text)) = parse_heading(line) {
                blocks.push(MarkdownBlock::Heading { level, text });
                index += 1;
                continue;
            }

            if matches!(line.trim(), "---" | "***" | "___") {
                blocks.push(MarkdownBlock::ThematicBreak);
                index += 1;
                continue;
            }

            if let Some(item) = unordered_item(line) {
                let mut items = vec![item.to_owned()];
                index += 1;
                while index < lines.len() {
                    let Some(item) = unordered_item(lines[index]) else {
                        break;
                    };
                    items.push(item.to_owned());
                    index += 1;
                }
                blocks.push(MarkdownBlock::List {
                    ordered: false,
                    items,
                });
                continue;
            }

            if let Some(item) = ordered_item(line) {
                let mut items = vec![item.to_owned()];
                index += 1;
                while index < lines.len() {
                    let Some(item) = ordered_item(lines[index]) else {
                        break;
                    };
                    items.push(item.to_owned());
                    index += 1;
                }
                blocks.push(MarkdownBlock::List {
                    ordered: true,
                    items,
                });
                continue;
            }

            if let Some(quote) = line.trim_start().strip_prefix('>') {
                let mut parts = vec![quote.trim_start()];
                index += 1;
                while index < lines.len() {
                    let Some(quote) = lines[index].trim_start().strip_prefix('>') else {
                        break;
                    };
                    parts.push(quote.trim_start());
                    index += 1;
                }
                blocks.push(MarkdownBlock::Quote(parts.join("\n")));
                continue;
            }

            let mut paragraph = vec![line.trim()];
            index += 1;
            while index < lines.len()
                && !lines[index].trim().is_empty()
                && !starts_block(lines[index])
            {
                paragraph.push(lines[index].trim());
                index += 1;
            }
            blocks.push(MarkdownBlock::Paragraph(paragraph.join(" ")));
        }

        Self { blocks }
    }
}

#[derive(Debug, Clone)]
pub struct MarkdownSurface {
    id: Uuid,
    path: PathBuf,
    source: Arc<String>,
    document: Arc<MarkdownDocument>,
    modified: Option<SystemTime>,
    file_backed: bool,
}

#[derive(Debug, Error)]
pub enum SurfaceError {
    #[error("reading surface content: {0}")]
    Io(#[from] std::io::Error),
}

impl MarkdownSurface {
    pub fn open(path: &Path) -> Result<Self, SurfaceError> {
        let source = fs::read_to_string(path)?;
        let modified = fs::metadata(path)?.modified().ok();
        Ok(Self {
            id: Uuid::new_v4(),
            path: path.into(),
            document: Arc::new(MarkdownDocument::parse(&source)),
            source: Arc::new(source),
            modified,
            file_backed: true,
        })
    }

    pub fn from_source(title: impl Into<PathBuf>, source: impl Into<String>) -> Self {
        let source = source.into();
        Self {
            id: Uuid::new_v4(),
            path: title.into(),
            document: Arc::new(MarkdownDocument::parse(&source)),
            source: Arc::new(source),
            modified: None,
            file_backed: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn document(&self) -> &MarkdownDocument {
        &self.document
    }

    pub fn reload(&mut self) -> Result<bool, SurfaceError> {
        if !self.file_backed {
            return Ok(false);
        }
        let metadata = fs::metadata(&self.path)?;
        let modified = metadata.modified().ok();
        // Some file systems have coarse timestamps, so compare contents as well.
        let source = fs::read_to_string(&self.path)?;
        let changed = source.as_str() != self.source.as_str();
        if changed {
            self.document = Arc::new(MarkdownDocument::parse(&source));
            self.source = Arc::new(source);
        }
        self.modified = modified;
        Ok(changed)
    }

    pub fn last_modified(&self) -> Option<SystemTime> {
        self.modified
    }
}

impl SurfaceBackend for MarkdownSurface {
    fn id(&self) -> Uuid {
        self.id
    }

    fn title(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Markdown")
    }

    fn render_text(&self) -> String {
        self.source.as_ref().clone()
    }

    fn kind(&self) -> SurfaceKind {
        SurfaceKind::Markdown
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn refresh(&mut self) -> Result<bool, SurfaceError> {
        self.reload()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    Unknown,
}

impl LogLevel {
    pub fn detect(line: &str) -> Self {
        let upper = line.to_ascii_uppercase();
        // Check higher severity first because messages can mention lower levels.
        for (needle, level) in [
            ("FATAL", Self::Fatal),
            ("PANIC", Self::Fatal),
            ("ERROR", Self::Error),
            ("ERR", Self::Error),
            ("WARN", Self::Warn),
            ("INFO", Self::Info),
            ("DEBUG", Self::Debug),
            ("TRACE", Self::Trace),
        ] {
            if contains_log_token(&upper, needle) {
                return level;
            }
        }
        Self::Unknown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub number: u64,
    pub level: LogLevel,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    pub query: Option<String>,
    pub levels: Option<Vec<LogLevel>>,
}

impl LogFilter {
    fn matches(&self, entry: &LogEntry) -> bool {
        let query_matches = self
            .query
            .as_ref()
            .is_none_or(|query| entry.text.to_lowercase().contains(&query.to_lowercase()));
        let level_matches = self
            .levels
            .as_ref()
            .is_none_or(|levels| levels.contains(&entry.level));
        query_matches && level_matches
    }
}

#[derive(Debug, Clone)]
pub struct LogSurface {
    id: Uuid,
    title: String,
    capacity: usize,
    lines: Arc<VecDeque<LogEntry>>,
    next_number: u64,
    filter: LogFilter,
    path: Option<PathBuf>,
    offset: u64,
    pending: Vec<u8>,
    file_prefix: Vec<u8>,
    history_bytes: usize,
}

impl LogSurface {
    pub fn new(title: impl Into<String>, capacity: usize) -> Self {
        Self {
            id: Uuid::new_v4(),
            title: title.into(),
            capacity: capacity.max(1),
            lines: Arc::new(VecDeque::new()),
            next_number: 1,
            filter: LogFilter::default(),
            path: None,
            offset: 0,
            pending: Vec::new(),
            file_prefix: Vec::new(),
            history_bytes: 0,
        }
    }

    /// Open a file and ingest its first bounded batch; refresh ingests the rest.
    pub fn open(path: &Path, capacity: usize) -> Result<Self, SurfaceError> {
        let title = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Log")
            .to_owned();
        let mut surface = Self::new(title, capacity);
        surface.path = Some(path.to_owned());
        surface.read_appended()?;
        Ok(surface)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn entries(&self) -> &VecDeque<LogEntry> {
        &self.lines
    }

    pub fn visible_entries(&self) -> impl Iterator<Item = &LogEntry> {
        self.lines.iter().filter(|entry| self.filter.matches(entry))
    }

    pub fn filter(&self) -> &LogFilter {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: LogFilter) {
        self.filter = filter;
    }

    pub fn clear_filter(&mut self) {
        self.filter = LogFilter::default();
    }

    pub fn push(&mut self, line: impl Into<String>) {
        let mut text = line.into();
        let mut end = text.len().min(MAX_LOG_LINE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        let entry = LogEntry {
            number: self.next_number,
            level: LogLevel::detect(&text),
            text,
        };
        self.next_number += 1;
        while self.lines.len() >= self.capacity
            || self.history_bytes + entry.text.len() > MAX_LOG_HISTORY_BYTES
        {
            if let Some(old) = Arc::make_mut(&mut self.lines).pop_front() {
                self.history_bytes -= old.text.len();
            }
        }
        self.history_bytes += entry.text.len();
        Arc::make_mut(&mut self.lines).push_back(entry);
    }

    /// Read bytes appended since the last call. A partial final line is held until a
    /// newline arrives. Truncation (common during log rotation) restarts at byte zero.
    pub fn read_appended(&mut self) -> Result<bool, SurfaceError> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let prefix_len = length.min(256) as usize;
        let mut prefix = vec![0; prefix_len];
        file.read_exact(&mut prefix)?;
        let comparable = self.file_prefix.len().min(prefix.len());
        let prefix_changed =
            comparable != 0 && self.file_prefix[..comparable] != prefix[..comparable];
        if length < self.offset || prefix_changed {
            self.offset = 0;
            self.pending.clear();
        }
        self.file_prefix = prefix;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.take(length.saturating_sub(self.offset).min(LOG_READ_BYTES))
            .read_to_end(&mut bytes)?;
        self.offset += bytes.len() as u64;
        if bytes.is_empty() {
            return Ok(false);
        }

        for byte in bytes {
            if byte == b'\n' {
                let mut line = self.pending.as_slice();
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                let line = String::from_utf8_lossy(line).into_owned();
                self.push(line);
                self.pending.clear();
            } else if self.pending.len() < MAX_LOG_LINE_BYTES {
                self.pending.push(byte);
            }
        }
        Ok(true)
    }
}

impl SurfaceBackend for LogSurface {
    fn id(&self) -> Uuid {
        self.id
    }

    fn title(&self) -> &str {
        &self.title
    }

    fn render_text(&self) -> String {
        self.visible_entries()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn kind(&self) -> SurfaceKind {
        SurfaceKind::Log
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn refresh(&mut self) -> Result<bool, SurfaceError> {
        self.read_appended()
    }

    fn search(&self, query: &str) -> Vec<SearchMatch> {
        if query.is_empty() {
            return Vec::new();
        }
        let query = query.to_lowercase();
        self.visible_entries()
            .filter(|entry| entry.text.to_lowercase().contains(&query))
            .map(|entry| SearchMatch {
                line: usize::try_from(entry.number).unwrap_or(usize::MAX),
                text: entry.text.clone(),
            })
            .collect()
    }
}

fn search_lines(text: &str, query: &str) -> Vec<SearchMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&query))
        .map(|(index, line)| SearchMatch {
            line: index + 1,
            text: line.to_owned(),
        })
        .collect()
}

fn parse_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim_start();
    let count = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&count) || trimmed.as_bytes().get(count) != Some(&b' ') {
        return None;
    }
    Some((count as u8, trimmed[count + 1..].trim().to_owned()))
}

fn unordered_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    ["- ", "* ", "+ "]
        .into_iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))
}

fn ordered_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0)
        .then(|| trimmed.get(digits..)?.strip_prefix(". "))
        .flatten()
}

fn starts_block(line: &str) -> bool {
    parse_heading(line).is_some()
        || line.trim_start().starts_with("```")
        || line.trim_start().starts_with('>')
        || unordered_item(line).is_some()
        || ordered_item(line).is_some()
        || matches!(line.trim(), "---" | "***" | "___")
}

fn contains_log_token(line: &str, needle: &str) -> bool {
    line.match_indices(needle).any(|(start, token)| {
        let end = start + token.len();
        let left = line[..start].chars().next_back();
        let right = line[end..].chars().next();
        left.is_none_or(|ch| !ch.is_ascii_alphanumeric())
            && right.is_none_or(|ch| !ch.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wmcterm-{name}-{}", Uuid::new_v4()))
    }

    #[test]
    fn log_surface_keeps_bounded_history() {
        let mut logs = LogSurface::new("server", 2);
        logs.push("one");
        logs.push("two");
        logs.push("three");
        assert_eq!(logs.render_text(), "two\nthree");
        assert_eq!(logs.entries().front().unwrap().number, 2);
    }

    #[test]
    fn log_reads_bounded_batches_without_skipping_or_repeating_bytes() {
        let path = temp_file("large.log");
        let line = "INFO bounded\n";
        let count = LOG_READ_BYTES as usize / line.len() + 100;
        fs::write(&path, line.repeat(count)).unwrap();
        let mut logs = LogSurface::open(&path, count).unwrap();
        assert_eq!(logs.offset, LOG_READ_BYTES);
        assert!(logs.read_appended().unwrap());
        assert_eq!(logs.entries().len(), count);
        assert_eq!(logs.next_number, count as u64 + 1);
        assert!(!logs.read_appended().unwrap());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn log_bounds_unterminated_lines_and_unicode_history() {
        let path = temp_file("unterminated.log");
        fs::write(&path, vec![b'x'; LOG_READ_BYTES as usize + 10]).unwrap();
        let mut logs = LogSurface::open(&path, 1000).unwrap();
        assert!(logs.pending.len() <= MAX_LOG_LINE_BYTES);
        logs.read_appended().unwrap();
        assert_eq!(logs.pending.len(), MAX_LOG_LINE_BYTES);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\nINFO next\n")
            .unwrap();
        logs.read_appended().unwrap();
        assert_eq!(logs.entries()[0].text.len(), MAX_LOG_LINE_BYTES);
        assert_eq!(logs.entries()[1].text, "INFO next");
        for _ in 0..200 {
            logs.push("中".repeat(MAX_LOG_LINE_BYTES));
        }
        assert!(logs.history_bytes <= MAX_LOG_HISTORY_BYTES);
        assert!(
            logs.entries()
                .iter()
                .all(|entry| entry.text.len() <= MAX_LOG_LINE_BYTES)
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn markdown_parser_builds_renderable_blocks() {
        let surface = MarkdownSurface::from_source(
            "README.md",
            "# wmcterm\n\nAgent workspace.\n\n- fast\n- focused\n\n```rust\nfn main() {}\n```\n> note",
        );
        assert_eq!(surface.document().blocks.len(), 5);
        assert_eq!(
            surface.document().blocks[0],
            MarkdownBlock::Heading {
                level: 1,
                text: "wmcterm".into()
            }
        );
        assert!(matches!(
            &surface.document().blocks[2],
            MarkdownBlock::List { ordered: false, items } if items.len() == 2
        ));
        assert!(matches!(
            &surface.document().blocks[3],
            MarkdownBlock::CodeBlock { language: Some(language), code }
                if language == "rust" && code == "fn main() {}"
        ));
    }

    #[test]
    fn markdown_refreshes_when_file_changes() {
        let path = temp_file("markdown.md");
        fs::write(&path, "# Before\n").unwrap();
        let mut surface = MarkdownSurface::open(&path).unwrap();
        fs::write(&path, "# After\n\nnew text\n").unwrap();
        assert!(surface.refresh().unwrap());
        assert!(surface.render_text().contains("After"));
        assert!(!surface.refresh().unwrap());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn source_markdown_is_not_implicitly_file_backed() {
        let path = temp_file("memory.md");
        fs::write(&path, "disk").unwrap();
        let mut surface = MarkdownSurface::from_source(&path, "memory");
        assert!(!surface.refresh().unwrap());
        assert_eq!(surface.render_text(), "memory");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn log_tails_partial_lines_and_handles_truncation() {
        let path = temp_file("tail.log");
        fs::write(&path, b"INFO ready\nWARN part").unwrap();
        let mut logs = LogSurface::open(&path, 10).unwrap();
        assert_eq!(logs.render_text(), "INFO ready");

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"ial\nERROR failed\n").unwrap();
        assert!(logs.refresh().unwrap());
        assert_eq!(logs.entries()[1].text, "WARN partial");
        assert_eq!(logs.entries()[2].level, LogLevel::Error);

        fs::write(&path, b"DEBUG rotated\n").unwrap();
        assert!(logs.refresh().unwrap());
        assert_eq!(logs.entries().back().unwrap().text, "DEBUG rotated");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn log_detects_replacement_that_is_longer_than_previous_file() {
        let path = temp_file("rotation.log");
        fs::write(&path, b"INFO old\n").unwrap();
        let mut logs = LogSurface::open(&path, 10).unwrap();
        fs::write(&path, b"WARN replacement is longer\n").unwrap();
        assert!(logs.refresh().unwrap());
        assert_eq!(
            logs.entries().back().unwrap().text,
            "WARN replacement is longer"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn log_filter_and_search_preserve_source_numbers() {
        let mut logs = LogSurface::new("server", 10);
        logs.push("INFO listening");
        logs.push("WARN slow request");
        logs.push("ERROR request failed");
        logs.set_filter(LogFilter {
            query: Some("request".into()),
            levels: Some(vec![LogLevel::Warn, LogLevel::Error]),
        });
        assert_eq!(
            logs.render_text(),
            "WARN slow request\nERROR request failed"
        );
        assert_eq!(logs.search("failed")[0].line, 3);
    }

    #[test]
    fn surfaces_share_refresh_search_and_kind_contract() {
        let mut surfaces: Vec<Box<dyn SurfaceBackend>> = vec![
            Box::new(MarkdownSurface::from_source("README.md", "# wmcterm")),
            Box::new(BrowserSurface::new("https://example.com")),
            Box::new(LogSurface::new("server", 10)),
        ];
        assert!(surfaces.iter().all(|surface| !surface.title().is_empty()));
        assert_eq!(surfaces[0].kind(), SurfaceKind::Markdown);
        assert_eq!(surfaces[0].search("wmcterm")[0].line, 1);
        assert!(!surfaces[1].refresh().unwrap());
    }
}
