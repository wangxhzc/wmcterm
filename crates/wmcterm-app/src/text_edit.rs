/// A small single-line editor whose cursor is always kept on a UTF-8 character
/// boundary. It deliberately owns no windowing state, so prompt behavior can be
/// tested without starting winit or a GPU.
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TextEditor {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    preedit: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextViewport {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) start_column: usize,
    pub(crate) columns: usize,
}

impl TextEditor {
    pub(crate) fn new(text: String) -> Self {
        let cursor = text.len();
        Self {
            text,
            cursor,
            anchor: None,
            preedit: String::new(),
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = None;
        self.preedit.clear();
    }

    pub(crate) fn cursor_display_column(&self) -> usize {
        UnicodeWidthStr::width(&self.text[..self.cursor])
    }

    pub(crate) fn cursor_byte(&self) -> usize {
        self.cursor
    }

    pub(crate) fn cursor_cell_width(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(1)
            .max(1)
    }

    /// Return the character-aligned horizontal window that keeps the caret (and
    /// an active IME preedit) visible. Text is never truncated in the model.
    pub(crate) fn viewport(&self, columns: usize) -> TextViewport {
        let columns = columns.max(1);
        let cursor_column = self.cursor_display_column();
        let caret_width = if self.preedit.is_empty() {
            self.cursor_cell_width()
        } else {
            UnicodeWidthStr::width(self.preedit.as_str()) + 2
        };
        let desired_start = (cursor_column + caret_width)
            .saturating_sub(columns)
            .min(cursor_column);
        let (start_byte, start_column) = boundary_at_or_before_column(&self.text, desired_start);
        let end_column = start_column + columns;
        let end_byte = boundary_at_or_before_column(&self.text, end_column).0;
        TextViewport {
            start_byte,
            end_byte,
            start_column,
            columns,
        }
    }

    pub(crate) fn preedit(&self) -> &str {
        &self.preedit
    }

    pub(crate) fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub(crate) fn selection_display_columns(&self) -> Option<(usize, usize)> {
        let range = self.selection_range()?;
        Some((
            UnicodeWidthStr::width(&self.text[..range.start]),
            UnicodeWidthStr::width(&self.text[..range.end]),
        ))
    }

    pub(crate) fn selected_text(&self) -> Option<&str> {
        self.selection_range().map(|range| &self.text[range])
    }

    pub(crate) fn select_all(&mut self) {
        self.clear_preedit();
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    pub(crate) fn take_selection(&mut self) -> Option<String> {
        let selected = self.selected_text()?.to_owned();
        self.delete_selection();
        Some(selected)
    }

    pub(crate) fn has_preedit(&self) -> bool {
        !self.preedit.is_empty()
    }

    /// Insert clipboard, key, or IME commit text. Prompts are single-line, so
    /// line separators become spaces and unrelated control characters are ignored.
    pub(crate) fn insert(&mut self, value: &str) {
        let value = single_line(value);
        self.delete_selection();
        self.text.insert_str(self.cursor, &value);
        self.cursor += value.len();
        self.preedit.clear();
    }

    pub(crate) fn set_preedit(&mut self, value: &str) {
        self.preedit = single_line(value);
    }

    pub(crate) fn clear_preedit(&mut self) {
        self.preedit.clear();
    }

    pub(crate) fn move_left_with_selection(&mut self, extend: bool) {
        self.clear_preedit();
        if !extend && let Some(range) = self.selection_range() {
            self.cursor = range.start;
            self.anchor = None;
            return;
        }
        self.begin_or_clear_selection(extend);
        self.cursor = previous_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_right_with_selection(&mut self, extend: bool) {
        self.clear_preedit();
        if !extend && let Some(range) = self.selection_range() {
            self.cursor = range.end;
            self.anchor = None;
            return;
        }
        self.begin_or_clear_selection(extend);
        self.cursor = next_boundary(&self.text, self.cursor);
    }

    pub(crate) fn move_home_with_selection(&mut self, extend: bool) {
        self.clear_preedit();
        self.begin_or_clear_selection(extend);
        self.cursor = 0;
    }

    pub(crate) fn move_end_with_selection(&mut self, extend: bool) {
        self.clear_preedit();
        self.begin_or_clear_selection(extend);
        self.cursor = self.text.len();
    }

    /// Place the cursor at the closest UTF-8 boundary for a monospace display
    /// column. Wide CJK/emoji cells choose the nearest edge rather than using
    /// byte or scalar counts.
    pub(crate) fn begin_pointer_selection(&mut self, column: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
            self.cursor_from_display_column_with_selection(column, true);
        } else {
            self.cursor_from_display_column_with_selection(column, false);
            self.anchor = Some(self.cursor);
        }
    }

    pub(crate) fn extend_pointer_selection(&mut self, column: usize) {
        self.anchor.get_or_insert(self.cursor);
        self.cursor_from_display_column_with_selection(column, true);
    }

    pub(crate) fn cursor_from_display_column_with_selection(
        &mut self,
        column: usize,
        extend: bool,
    ) -> usize {
        self.clear_preedit();
        self.begin_or_clear_selection(extend);
        let mut display_column = 0;
        for (index, character) in self.text.char_indices() {
            let width = UnicodeWidthChar::width(character).unwrap_or(0);
            let next = display_column + width;
            if column < next {
                self.cursor = if (column - display_column) * 2 < width {
                    index
                } else {
                    index + character.len_utf8()
                };
                return self.cursor;
            }
            display_column = next;
        }
        self.cursor = self.text.len();
        self.cursor
    }

    pub(crate) fn backspace(&mut self) {
        self.clear_preedit();
        if self.delete_selection() {
            return;
        }
        let previous = previous_boundary(&self.text, self.cursor);
        if previous != self.cursor {
            self.text.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    pub(crate) fn delete(&mut self) {
        self.clear_preedit();
        if self.delete_selection() {
            return;
        }
        let next = next_boundary(&self.text, self.cursor);
        if next != self.cursor {
            self.text.drain(self.cursor..next);
        }
    }

    /// Test-only model of the visible editor text. Production rendering uses
    /// colored spans plus a separate caret rectangle.
    #[cfg(test)]
    pub(crate) fn display_text(&self) -> String {
        let mut display = String::with_capacity(self.text.len() + self.preedit.len() + 2);
        display.push_str(&self.text[..self.cursor]);
        if !self.preedit.is_empty() {
            display.push('「');
            display.push_str(&self.preedit);
            display.push('」');
        }
        display.push_str(&self.text[self.cursor..]);
        display
    }

    fn begin_or_clear_selection(&mut self, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
    }

    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            self.anchor = None;
            return false;
        };
        self.cursor = range.start;
        self.text.drain(range);
        self.anchor = None;
        true
    }
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .next_back()
        .map_or(cursor, |(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .chars()
        .next()
        .map_or(cursor, |character| cursor + character.len_utf8())
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .filter_map(|character| match character {
            '\r' => None,
            '\n' | '\t' => Some(' '),
            value if value.is_control() => None,
            value => Some(value),
        })
        .collect()
}

fn boundary_at_or_before_column(text: &str, target: usize) -> (usize, usize) {
    let mut column = 0;
    for (byte, character) in text.char_indices() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if column + width > target {
            return (byte, column);
        }
        column += width;
    }
    (text.len(), column)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_at_unicode_boundaries() {
        let mut editor = TextEditor::new("A中🙂Z".into());
        editor.move_left_with_selection(false);
        editor.backspace();
        assert_eq!(editor.text(), "A中Z");
        assert!(editor.text().is_char_boundary(editor.cursor));
        editor.move_left_with_selection(false);
        editor.delete();
        assert_eq!(editor.text(), "AZ");
    }

    #[test]
    fn supports_home_end_and_middle_insertion() {
        let mut editor = TextEditor::new("世界".into());
        editor.move_home_with_selection(false);
        editor.insert("你好");
        editor.move_end_with_selection(false);
        editor.insert("!");
        assert_eq!(editor.text(), "你好世界!");
    }

    #[test]
    fn moving_cursor_does_not_change_display_text_or_width() {
        let mut editor = TextEditor::new("abc中文".into());
        let display = editor.display_text();
        let width = UnicodeWidthStr::width(display.as_str());
        editor.move_left_with_selection(false);
        editor.move_left_with_selection(false);
        editor.move_home_with_selection(false);
        assert_eq!(editor.display_text(), display);
        assert_eq!(
            UnicodeWidthStr::width(editor.display_text().as_str()),
            width
        );
    }

    #[test]
    fn preedit_is_not_committed_and_paste_stays_single_line() {
        let mut editor = TextEditor::new("a".into());
        editor.set_preedit("拼音");
        assert_eq!(editor.text(), "a");
        assert_eq!(editor.display_text(), "a「拼音」");
        editor.insert("中文\r\nnext\tvalue\u{7}");
        assert_eq!(editor.text(), "a中文 next value");
        assert!(!editor.has_preedit());
    }

    #[test]
    fn display_column_clicks_respect_wide_unicode_cells() {
        let mut editor = TextEditor::new("a中🙂z".into());
        assert_eq!(
            editor.cursor_from_display_column_with_selection(0, false),
            0
        );
        assert_eq!(
            editor.cursor_from_display_column_with_selection(1, false),
            1
        );
        assert_eq!(
            editor.cursor_from_display_column_with_selection(2, false),
            4
        );
        assert_eq!(editor.cursor_display_column(), 3);
        assert_eq!(
            editor.cursor_from_display_column_with_selection(4, false),
            8
        );
        assert_eq!(
            editor.cursor_from_display_column_with_selection(99, false),
            editor.text().len()
        );
        assert!(editor.text().is_char_boundary(editor.cursor));
        editor.clear();
        assert_eq!(editor.cursor_display_column(), 0);
        assert_eq!(editor.display_text(), "");
    }

    #[test]
    fn selection_replaces_unicode_and_supports_copy_cut_navigation() {
        let mut editor = TextEditor::new("A中文Z".into());
        editor.select_all();
        assert_eq!(editor.selected_text(), Some("A中文Z"));
        assert_eq!(editor.selection_display_columns(), Some((0, 6)));
        editor.insert("替换");
        assert_eq!(editor.text(), "替换");

        editor.move_home_with_selection(false);
        editor.move_right_with_selection(true);
        assert_eq!(editor.selected_text(), Some("替"));
        assert_eq!(editor.take_selection().as_deref(), Some("替"));
        assert_eq!(editor.text(), "换");

        editor.select_all();
        editor.move_left_with_selection(false);
        assert!(editor.selection_range().is_none());
        assert_eq!(editor.cursor, 0);
    }

    #[test]
    fn viewport_follows_long_unicode_text_without_splitting_characters() {
        let mut editor = TextEditor::new("ab中文🙂xyz".into());
        let view = editor.viewport(5);
        assert_eq!(&editor.text()[view.start_byte..view.end_byte], "🙂xyz");
        assert_eq!(view.start_column, 6);
        assert!(editor.text().is_char_boundary(view.start_byte));
        assert!(editor.text().is_char_boundary(view.end_byte));

        editor.move_home_with_selection(false);
        let view = editor.viewport(5);
        assert_eq!(view.start_byte, 0);
        assert_eq!(&editor.text()[..view.end_byte], "ab中");
    }

    #[test]
    fn viewport_reserves_room_for_ime_preedit() {
        let mut editor = TextEditor::new("123456".into());
        editor.set_preedit("中文");
        let view = editor.viewport(6);
        assert_eq!(view.start_column, 6);
        assert_eq!(view.start_byte, editor.text().len());
    }
}
