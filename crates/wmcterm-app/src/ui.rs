use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Bounds {
    pub(crate) fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

pub(crate) fn bounds_at(items: &[Bounds], x: f32, y: f32) -> Option<usize> {
    items.iter().position(|bounds| bounds.contains(x, y))
}

pub(crate) fn titlebar_button_layout(window_width: f32, count: usize) -> Vec<Bounds> {
    const BUTTON_SIZE: f32 = 40.0;
    let start = (window_width - count as f32 * BUTTON_SIZE).max(0.0);
    (0..count)
        .map(|index| Bounds {
            x: start + index as f32 * BUTTON_SIZE,
            y: 0.0,
            width: BUTTON_SIZE,
            height: BUTTON_SIZE,
        })
        .collect()
}

/// Sidebar control at the leading edge, window and notification controls at
/// the trailing edge. The returned order matches the titlebar action order.
pub(crate) fn titlebar_chrome_layout(window_width: f32) -> Vec<Bounds> {
    let mut bounds = vec![Bounds {
        x: 0.0,
        y: 0.0,
        width: 40.0,
        height: 40.0,
    }];
    bounds.extend(titlebar_button_layout(window_width, 4));
    bounds
}

pub(crate) fn centered_text_left(bounds: Bounds, text: &str, font_size: f32) -> f32 {
    let text_width = display_width(text) as f32 * font_size * 0.6;
    bounds.x + ((bounds.width - text_width) / 2.0).max(0.0)
}

pub(crate) fn centered_truncated_text_left(
    bounds: Bounds,
    text: &str,
    font_size: f32,
) -> (String, f32) {
    if font_size <= 0.0 {
        return (text.to_owned(), centered_text_left(bounds, text, 1.0));
    }
    let inner_width = (bounds.width - 8.0).max(font_size * 0.6);
    let max_columns = (inner_width / (font_size * 0.6)).floor().max(1.0) as usize;
    let visible = truncate_display_width(text, max_columns);
    let left = centered_text_left(bounds, &visible, font_size);
    (visible, left)
}

pub(crate) fn scroll_selection(index: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    index
        .min(len - 1)
        .saturating_add_signed(delta.signum())
        .min(len - 1)
}

pub(crate) fn centered_panel(
    window_width: f32,
    window_height: f32,
    preferred_width: f32,
    preferred_height: f32,
    margin: f32,
) -> Bounds {
    let available_width = (window_width - margin * 2.0).max(1.0);
    let available_height = (window_height - margin * 2.0).max(1.0);
    let width = preferred_width.min(available_width);
    let height = preferred_height.min(available_height);
    Bounds {
        x: ((window_width - width) / 2.0).max(margin.min(window_width / 2.0)),
        y: ((window_height - height) / 2.0).max(margin.min(window_height / 2.0)),
        width,
        height,
    }
}

pub(crate) fn scrolled_range(count: usize, offset: usize, capacity: usize) -> Range<usize> {
    if count == 0 || capacity == 0 {
        return 0..0;
    }
    let capacity = capacity.min(count);
    let start = offset.min(count - capacity);
    start..start + capacity
}

pub(crate) fn palette_rows(panel: Bounds, count: usize) -> Vec<Bounds> {
    const LINE_HEIGHT: f32 = 25.0;
    let first_line_top = panel.y + 8.0;
    (0..count)
        .map(|index| Bounds {
            x: panel.x + 10.0,
            y: first_line_top + index as f32 * LINE_HEIGHT,
            width: (panel.width - 28.0).max(1.0),
            height: LINE_HEIGHT,
        })
        .collect()
}

pub(crate) fn list_scrollbar(
    panel: Bounds,
    total: usize,
    visible: usize,
    scroll: usize,
) -> Option<(Bounds, Bounds)> {
    if total <= visible || visible == 0 {
        return None;
    }
    let track = Bounds {
        x: panel.x + panel.width - 7.0,
        y: panel.y + 6.0,
        width: 3.0,
        height: (panel.height - 12.0).max(1.0),
    };
    let thumb_height = (track.height * visible as f32 / total as f32).max(18.0);
    let max_scroll = total - visible;
    let travel = (track.height - thumb_height).max(0.0);
    let thumb = Bounds {
        y: track.y + travel * scroll.min(max_scroll) as f32 / max_scroll as f32,
        height: thumb_height,
        ..track
    };
    Some((track, thumb))
}

pub(crate) fn palette_row_at(panel: Bounds, count: usize, x: f32, y: f32) -> Option<usize> {
    palette_rows(panel, count)
        .iter()
        .position(|row| row.contains(x, y))
}

pub(crate) fn fixed_row_at(
    y: f32,
    top: f32,
    row_height: f32,
    visible_count: usize,
) -> Option<usize> {
    if y < top || row_height <= 0.0 {
        return None;
    }
    let index = ((y - top) / row_height) as usize;
    (index < visible_count).then_some(index)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NotificationRow {
    pub bounds: Bounds,
    pub accent: Bounds,
    pub content: Bounds,
    pub close: Bounds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationRowHit {
    Open(usize),
    Close(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentApprovalCard {
    pub bounds: Bounds,
    pub content: Bounds,
    pub buttons: Vec<Bounds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentApprovalHit {
    pub card: usize,
    pub action: usize,
}

pub(crate) fn agent_approval_cards(
    region: Bounds,
    action_counts: &[usize],
) -> Vec<AgentApprovalCard> {
    const PADDING: f32 = 8.0;
    const CARD_HEIGHT: f32 = 112.0;
    const CARD_GAP: f32 = 8.0;
    const BUTTON_HEIGHT: f32 = 28.0;
    const BUTTON_GAP: f32 = 6.0;

    action_counts
        .iter()
        .enumerate()
        .map(|(index, action_count)| {
            let bounds = Bounds {
                x: region.x + PADDING,
                y: region.y + PADDING + index as f32 * (CARD_HEIGHT + CARD_GAP),
                width: (region.width - PADDING * 2.0).max(1.0),
                height: CARD_HEIGHT,
            };
            let action_count = (*action_count).max(1);
            let button_width = ((bounds.width
                - PADDING * 2.0
                - BUTTON_GAP * action_count.saturating_sub(1) as f32)
                / action_count as f32)
                .max(1.0);
            let button_y = bounds.y + bounds.height - BUTTON_HEIGHT - PADDING;
            let buttons = (0..action_count)
                .map(|action| Bounds {
                    x: bounds.x + PADDING + action as f32 * (button_width + BUTTON_GAP),
                    y: button_y,
                    width: button_width,
                    height: BUTTON_HEIGHT,
                })
                .collect();
            AgentApprovalCard {
                bounds,
                content: Bounds {
                    x: bounds.x + 12.0,
                    y: bounds.y + 8.0,
                    width: (bounds.width - 24.0).max(1.0),
                    height: (button_y - bounds.y - 12.0).max(1.0),
                },
                buttons,
            }
        })
        .take_while(|card| card.bounds.y + card.bounds.height <= region.y + region.height - PADDING)
        .collect()
}

pub(crate) fn agent_approval_hit(
    region: Bounds,
    action_counts: &[usize],
    x: f32,
    y: f32,
) -> Option<AgentApprovalHit> {
    agent_approval_cards(region, action_counts)
        .iter()
        .enumerate()
        .find_map(|(card, layout)| {
            layout
                .buttons
                .iter()
                .position(|button| button.contains(x, y))
                .map(|action| AgentApprovalHit { card, action })
        })
}

pub(crate) fn notification_row_layouts(panel: Bounds, count: usize) -> Vec<NotificationRow> {
    const PANEL_PADDING: f32 = 8.0;
    const TEXT_TOP_PADDING: f32 = 16.0;
    const HEADER_LINES: f32 = 2.0;
    const LINE_HEIGHT: f32 = 21.0;
    const ROW_STRIDE: f32 = 96.0;
    const ROW_HEIGHT: f32 = 92.0;
    const CLOSE_SIZE: f32 = 24.0;

    let top = panel.y + TEXT_TOP_PADDING + HEADER_LINES * LINE_HEIGHT;
    (0..count)
        .map(|index| {
            let bounds = Bounds {
                x: panel.x + PANEL_PADDING,
                y: top + index as f32 * ROW_STRIDE,
                width: (panel.width - PANEL_PADDING * 2.0).max(0.0),
                height: ROW_HEIGHT,
            };
            let close = Bounds {
                x: bounds.x + (bounds.width - CLOSE_SIZE - 6.0).max(0.0),
                y: bounds.y + (bounds.height - CLOSE_SIZE) / 2.0,
                width: CLOSE_SIZE,
                height: CLOSE_SIZE,
            };
            NotificationRow {
                bounds,
                accent: Bounds {
                    x: bounds.x,
                    y: bounds.y + 6.0,
                    width: 3.0,
                    height: bounds.height - 12.0,
                },
                content: Bounds {
                    x: bounds.x + 12.0,
                    y: bounds.y + 5.0,
                    width: (close.x - bounds.x - 18.0).max(1.0),
                    height: bounds.height - 10.0,
                },
                close,
            }
        })
        .take_while(|row| {
            row.bounds.y + row.bounds.height <= panel.y + panel.height - PANEL_PADDING
        })
        .collect()
}

pub(crate) fn notification_mark_all_read_button(panel: Bounds) -> Bounds {
    const WIDTH: f32 = 104.0;
    Bounds {
        x: (panel.x + panel.width - WIDTH - 12.0).max(panel.x + 8.0),
        y: panel.y + 10.0,
        width: WIDTH.min((panel.width - 20.0).max(1.0)),
        height: 28.0,
    }
}

pub(crate) fn notification_mark_all_read_hit(panel: Bounds, enabled: bool, x: f32, y: f32) -> bool {
    enabled && notification_mark_all_read_button(panel).contains(x, y)
}

pub(crate) fn notification_rows(panel: Bounds, count: usize) -> Vec<Bounds> {
    notification_row_layouts(panel, count)
        .into_iter()
        .map(|row| row.bounds)
        .collect()
}

pub(crate) fn notification_row_at(
    panel: Bounds,
    count: usize,
    x: f32,
    y: f32,
) -> Option<NotificationRowHit> {
    notification_row_layouts(panel, count)
        .iter()
        .enumerate()
        .find_map(|(index, row)| {
            row.close
                .contains(x, y)
                .then_some(NotificationRowHit::Close(index))
                .or_else(|| {
                    row.bounds
                        .contains(x, y)
                        .then_some(NotificationRowHit::Open(index))
                })
        })
}

/// Position a compact pointer-anchored menu without allowing it to escape the
/// window. The same bounds are used for drawing and hit testing.
pub(crate) fn popup_menu(
    anchor_x: f32,
    anchor_y: f32,
    window_width: f32,
    window_height: f32,
    preferred_width: f32,
    row_height: f32,
    rows: usize,
) -> Bounds {
    let margin = 8.0;
    let width = preferred_width.min((window_width - margin * 2.0).max(1.0));
    let height = (row_height * rows as f32).min((window_height - margin * 2.0).max(1.0));
    Bounds {
        x: anchor_x.min(window_width - width - margin).max(margin),
        y: anchor_y.min(window_height - height - margin).max(margin),
        width,
        height,
    }
}

pub(crate) fn menu_row_at(
    menu: Bounds,
    x: f32,
    y: f32,
    row_height: f32,
    rows: usize,
) -> Option<usize> {
    if !menu.contains(x, y) {
        return None;
    }
    fixed_row_at(y, menu.y, row_height, rows)
}

pub(crate) const TAB_OVERFLOW_ROW_HEIGHT: f32 = 30.0;
const TAB_OVERFLOW_COLUMN_WIDTH: f32 = 9.0;
const TAB_OVERFLOW_HORIZONTAL_PADDING: f32 = 28.0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TabOverflowMenuLayout {
    pub(crate) panel: Bounds,
    pub(crate) visible: Range<usize>,
    pub(crate) text_columns: usize,
}

impl TabOverflowMenuLayout {
    pub(crate) fn surface_index(&self, visible_row: usize) -> Option<usize> {
        (visible_row < self.visible.len()).then_some(self.visible.start + visible_row)
    }

    pub(crate) fn line(&self, lines: &[String], surface_index: usize) -> String {
        lines
            .get(surface_index)
            .map(|line| truncate_display_width(line, self.text_columns))
            .unwrap_or_default()
    }
}

pub(crate) fn tab_overflow_menu_capacity(window_height: f32, count: usize) -> usize {
    let available_height = (window_height - 16.0).max(1.0);
    ((available_height / TAB_OVERFLOW_ROW_HEIGHT).floor() as usize)
        .max(1)
        .min(count)
}

pub(crate) fn tab_overflow_initial_offset(count: usize, capacity: usize, active: usize) -> usize {
    if count == 0 || capacity == 0 || active < capacity {
        0
    } else {
        active.min(count - 1) + 1 - capacity.min(count)
    }
}

pub(crate) fn tab_overflow_scroll_offset(
    count: usize,
    capacity: usize,
    offset: usize,
    delta: isize,
) -> usize {
    let max_offset = count.saturating_sub(capacity.min(count));
    offset.saturating_add_signed(delta).min(max_offset)
}

pub(crate) fn tab_overflow_menu_layout(
    anchor_x: f32,
    anchor_y: f32,
    window_width: f32,
    window_height: f32,
    lines: &[String],
    offset: usize,
) -> TabOverflowMenuLayout {
    let capacity = tab_overflow_menu_capacity(window_height, lines.len());
    let visible = scrolled_range(lines.len(), offset, capacity);
    let preferred_width = lines
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()) as f32 * TAB_OVERFLOW_COLUMN_WIDTH)
        .fold(120.0, f32::max)
        + TAB_OVERFLOW_HORIZONTAL_PADDING;
    let panel = popup_menu(
        anchor_x,
        anchor_y,
        window_width,
        window_height,
        preferred_width,
        TAB_OVERFLOW_ROW_HEIGHT,
        visible.len(),
    );
    let text_columns = ((panel.width - TAB_OVERFLOW_HORIZONTAL_PADDING).max(0.0)
        / TAB_OVERFLOW_COLUMN_WIDTH)
        .floor() as usize;
    TabOverflowMenuLayout {
        panel,
        visible,
        text_columns,
    }
}

pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 1 {
        return "…".chars().take(max_chars).collect();
    }
    let mut result: String = text.chars().take(max_chars - 1).collect();
    result.push('…');
    result
}

pub(crate) fn truncate_display_width(text: &str, max_columns: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_columns {
        return text.to_owned();
    }
    if max_columns == 0 {
        return String::new();
    }
    let content_columns = max_columns - 1;
    let mut width = 0;
    let mut result = String::new();
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_columns {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push('…');
    result
}

pub(crate) fn truncate_display_width_start(text: &str, max_columns: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_columns {
        return text.to_owned();
    }
    if max_columns == 0 {
        return String::new();
    }
    if max_columns == 1 {
        return "…".into();
    }
    let mut suffix = Vec::new();
    let mut width = 1;
    for character in text.chars().rev() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > max_columns {
            break;
        }
        suffix.push(character);
        width += character_width;
    }
    suffix.reverse();
    format!("…{}", suffix.into_iter().collect::<String>())
}

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Wrap notification text into display-column bounded lines. Newlines and
/// repeated whitespace are normalized so previews remain compact and stable.
pub(crate) fn notification_lines(text: &str, max_columns: usize) -> Vec<String> {
    if max_columns == 0 {
        return Vec::new();
    }
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut width = 0;
    for character in normalized.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if !line.is_empty() && width + character_width > max_columns {
            lines.push(std::mem::take(&mut line));
            width = 0;
        }
        line.push(character);
        width += character_width;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

pub(crate) fn notification_preview(
    text: &str,
    max_columns: usize,
    line_count: usize,
    scroll_line: Option<usize>,
) -> Vec<String> {
    if line_count == 0 || max_columns == 0 {
        return Vec::new();
    }
    let lines = notification_lines(text, max_columns);
    let start = scroll_line.map_or(0, |line| line % lines.len().max(1));
    let mut visible: Vec<_> = lines.iter().skip(start).take(line_count).cloned().collect();
    if scroll_line.is_none() && lines.len() > line_count {
        let last = visible.len().saturating_sub(1);
        visible[last] = format!(
            "{}…",
            truncate_display_width(&visible[last], max_columns.saturating_sub(1))
                .trim_end_matches('…')
        );
    }
    visible
}

/// Build the exact monospace label rendered in a surface tab. Titles are
/// bounded by display columns so CJK and Latin labels consume equal geometry.
#[cfg(test)]
fn surface_tab_label(title: &str, index: usize, agent: bool) -> String {
    format!("{} {}", surface_tab_title(title, agent), index + 1)
}

/// Keep an Agent's transient braille spinner in a fixed two-column slot. The
/// slot remains reserved between frames (and while idle), so OSC title changes
/// do not resize the tab or displace the stable title.
pub(crate) fn surface_tab_title(title: &str, agent: bool) -> String {
    let (spinner, stable) = split_agent_spinner(title);
    let stable = stable.trim_end_matches(char::is_whitespace);
    if agent || spinner.is_some() {
        format!("{} {}", spinner.unwrap_or(' '), stable)
    } else {
        stable.to_owned()
    }
}

/// Fit the title to the tab's current text bounds before it reaches the text
/// shaper. This prevents a squeezed trailing word from wrapping onto a clipped
/// second line and leaving the visible first line mostly empty.
pub(crate) fn surface_tab_visible_title(
    title: &str,
    agent: bool,
    available_width: f32,
    cell_width: f32,
) -> String {
    let max_columns = (available_width / cell_width.max(1.0)).floor().max(1.0) as usize;
    truncate_display_width(&surface_tab_title(title, agent), max_columns)
}

fn split_agent_spinner(title: &str) -> (Option<char>, &str) {
    let trimmed = title.trim_start();
    let mut characters = trimmed.chars();
    let Some(first) = characters.next() else {
        return (None, trimmed);
    };
    let remainder = characters.as_str();
    if ('\u{2800}'..='\u{28ff}').contains(&first)
        && remainder.chars().next().is_some_and(char::is_whitespace)
        && !remainder.trim().is_empty()
    {
        (Some(first), remainder.trim_start())
    } else {
        (None, trimmed)
    }
}

/// Size the visible label plus its leading inset, a small trailing gap, and
/// the close target. Keeping this beside the label prevents empty click areas
/// when a title is short or contains wide characters.
const SURFACE_TAB_LEFT_INSET: f32 = 7.0;
const SURFACE_TAB_TITLE_NUMBER_GAP: f32 = 4.0;
const SURFACE_TAB_NUMBER_CLOSE_GAP: f32 = 4.0;
const SURFACE_TAB_NUMBER_SLACK: f32 = 4.0;
const SURFACE_TAB_MAX_CLOSE_WIDTH: f32 = 24.0;

#[cfg(test)]
fn surface_tab_width_at(title: &str, index: usize, cell_width: f32, agent: bool) -> f32 {
    let title_columns = display_width(&surface_tab_title(title, agent)) as f32;
    let number_columns = display_width(&(index + 1).to_string()) as f32;
    (SURFACE_TAB_LEFT_INSET
        + title_columns * cell_width
        + SURFACE_TAB_TITLE_NUMBER_GAP
        + number_columns * cell_width
        + SURFACE_TAB_NUMBER_SLACK
        + SURFACE_TAB_NUMBER_CLOSE_GAP
        + SURFACE_TAB_MAX_CLOSE_WIDTH)
        .max(64.0)
}

pub(crate) fn surface_tab_text_bounds(
    tab: Bounds,
    close: Option<Bounds>,
    index: usize,
    cell_width: f32,
) -> (Bounds, Bounds) {
    let tab_right = tab.x + tab.width;
    let close_left = close
        .map_or(tab_right, |close| close.x)
        .clamp(tab.x, tab_right);
    let number_columns = display_width(&(index + 1).to_string()) as f32;
    let number_width = (number_columns * cell_width + SURFACE_TAB_NUMBER_SLACK)
        .min((close_left - tab.x - SURFACE_TAB_NUMBER_CLOSE_GAP).max(1.0));
    let number_left = (close_left - SURFACE_TAB_NUMBER_CLOSE_GAP - number_width).max(tab.x);
    let title_left = (tab.x + SURFACE_TAB_LEFT_INSET).min(number_left);
    (
        Bounds {
            x: title_left,
            y: tab.y,
            width: (number_left - SURFACE_TAB_TITLE_NUMBER_GAP - title_left).max(1.0),
            height: tab.height,
        },
        Bounds {
            x: number_left,
            y: tab.y,
            width: number_width,
            height: tab.height,
        },
    )
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SidebarRow {
    pub index: usize,
    pub bounds: Bounds,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SidebarLayout {
    pub rows: Vec<SidebarRow>,
    pub new_workspace: Bounds,
    pub scroll: usize,
    pub max_scroll: usize,
}

/// Lay out the scrollable workspace rows and the fixed creation affordance.
/// The returned rectangles are shared by rendering and hit testing.
pub(crate) fn sidebar_layout(
    width: f32,
    height: f32,
    row_heights: &[f32],
    requested_scroll: usize,
) -> SidebarLayout {
    let rows_top = 88.0;
    // Keep a reliable context-menu target below the final workspace row.
    // This remains available even when the workspace list needs to scroll.
    let footer_height = 32.0;
    let available = (height - rows_top - footer_height).max(1.0);
    let mut max_scroll = row_heights.len();
    let mut tail_height = 0.0;
    while max_scroll > 0 {
        let candidate = row_heights[max_scroll - 1].max(1.0);
        if max_scroll < row_heights.len() && tail_height + candidate > available {
            break;
        }
        max_scroll -= 1;
        tail_height += candidate;
    }
    let scroll = requested_scroll.min(max_scroll);
    let mut top = rows_top;
    let mut rows = Vec::new();
    for (index, height) in row_heights.iter().copied().enumerate().skip(scroll) {
        let height = height.max(1.0);
        if top + height > rows_top + available {
            break;
        }
        rows.push(SidebarRow {
            index,
            bounds: Bounds {
                x: 0.0,
                y: top,
                width,
                height,
            },
        });
        top += height;
    }
    SidebarLayout {
        rows,
        new_workspace: Bounds {
            x: (width - 40.0).max(0.0),
            y: 49.0,
            width: 32.0_f32.min(width),
            height: 30.0,
        },
        scroll,
        max_scroll,
    }
}

/// Return the visible row position selected by a vertical workspace drag.
/// A reorder only happens after the pointer crosses a neighbouring row's
/// midpoint, which keeps the list stable around row boundaries.
pub(crate) fn workspace_drag_target(rows: &[SidebarRow], source: usize, y: f32) -> usize {
    let mut target = source.min(rows.len().saturating_sub(1));
    for (index, row) in rows.iter().enumerate() {
        let midpoint = row.bounds.y + row.bounds.height / 2.0;
        if index < source && y < midpoint {
            return index;
        }
        if index > source && y > midpoint {
            target = index;
        }
    }
    target
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SurfaceTab {
    pub index: usize,
    pub bounds: Bounds,
    pub close: Option<Bounds>,
}

pub(crate) const SURFACE_TAB_OVERFLOW_WIDTH: f32 = 44.0;

pub(crate) fn surface_tab_overflow_bounds(
    pane: Bounds,
    visible_tabs: usize,
    total_tabs: usize,
) -> Option<Bounds> {
    if total_tabs <= visible_tabs {
        return None;
    }
    let width = SURFACE_TAB_OVERFLOW_WIDTH.min((pane.width - 28.0).max(1.0));
    Some(Bounds {
        x: (pane.x + pane.width - 28.0 - width).max(pane.x),
        y: pane.y,
        width,
        height: 25.0,
    })
}

/// Give visible tabs equal, comfortable widths while retaining the active tab.
/// Constrained panes show a stable page of tabs and reserve an overflow target;
/// every visible surface keeps a close target so a pane may remain empty.
pub(crate) fn surface_tabs_layout(
    pane: Bounds,
    tab_count: usize,
    active: usize,
) -> Vec<SurfaceTab> {
    if tab_count == 0 || pane.width <= 32.0 {
        return Vec::new();
    }
    let active = active.min(tab_count - 1);
    let left = pane.x;
    let full_available = (pane.width - 28.0).max(1.0);
    let unconstrained_count = (full_available / 80.0).floor().max(1.0) as usize;
    let overflow_width = if tab_count > unconstrained_count {
        SURFACE_TAB_OVERFLOW_WIDTH.min(full_available)
    } else {
        0.0
    };
    let available = (full_available - overflow_width).max(1.0);
    let minimum_width = 80.0_f32.min(available);
    let visible_count = ((available / minimum_width).floor() as usize).clamp(1, tab_count);
    let start = active / visible_count * visible_count;
    let end = (start + visible_count).min(tab_count);
    let width = (available / (end - start) as f32).min(240.0);
    let mut x = left;
    (start..end)
        .map(|index| {
            let bounds = Bounds {
                x,
                y: pane.y,
                width,
                height: 25.0,
            };
            x += width;
            let close_width = (width * 0.22)
                .clamp(16.0, SURFACE_TAB_MAX_CLOSE_WIDTH)
                .min(width);
            SurfaceTab {
                index,
                bounds,
                close: Some(Bounds {
                    x: bounds.x + bounds.width - close_width,
                    y: bounds.y,
                    width: close_width,
                    height: bounds.height,
                }),
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScrollbarLayout {
    pub track: Bounds,
    pub thumb: Bounds,
    pub max_offset: usize,
    pub page_rows: usize,
}

impl ScrollbarLayout {
    /// Convert a dragged thumb position to the terminal viewport offset.
    /// Offset zero is the live bottom; max offset is the oldest history.
    pub(crate) fn offset_for_pointer(self, pointer_y: f32, grab_y: f32) -> usize {
        let travel = (self.track.height - self.thumb.height).max(0.0);
        if travel <= f32::EPSILON || self.max_offset == 0 {
            return 0;
        }
        let top = (pointer_y - grab_y).clamp(self.track.y, self.track.y + travel);
        let progress_from_top = (top - self.track.y) / travel;
        ((1.0 - progress_from_top) * self.max_offset as f32).round() as usize
    }

    pub(crate) fn page_delta_at(self, y: f32) -> Option<isize> {
        if !self
            .track
            .contains(self.track.x + self.track.width / 2.0, y)
            || self
                .thumb
                .contains(self.thumb.x + self.thumb.width / 2.0, y)
        {
            return None;
        }
        Some(if y < self.thumb.y {
            -(self.page_rows as isize)
        } else {
            self.page_rows as isize
        })
    }
}

/// Geometry for the interactive terminal scrollbar. Rendering, hit testing,
/// track paging and thumb dragging all consume this exact value.
pub(crate) fn terminal_scrollbar_layout(
    pane: Bounds,
    visible_rows: usize,
    scrollback_rows: usize,
    viewport_offset: usize,
) -> Option<ScrollbarLayout> {
    if scrollback_rows == 0 || pane.width < 20.0 || pane.height <= 31.0 {
        return None;
    }
    let track = Bounds {
        x: pane.x + pane.width - 10.0,
        y: pane.y + 25.0,
        width: 8.0,
        height: pane.height - 31.0,
    };
    let total = scrollback_rows.saturating_add(visible_rows.max(1));
    let thumb_height = (track.height * visible_rows.max(1) as f32 / total as f32)
        .clamp(20.0_f32.min(track.height), track.height);
    let travel = track.height - thumb_height;
    let offset = viewport_offset.min(scrollback_rows);
    let progress_from_top = (scrollback_rows - offset) as f32 / scrollback_rows as f32;
    Some(ScrollbarLayout {
        track,
        thumb: Bounds {
            x: track.x,
            y: track.y + travel * progress_from_top,
            width: track.width,
            height: thumb_height,
        },
        max_offset: scrollback_rows,
        page_rows: visible_rows.max(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titlebar_buttons_and_centered_labels_share_the_same_geometry() {
        let buttons = titlebar_button_layout(1000.0, 5);
        assert_eq!(buttons.first().unwrap().x, 800.0);
        assert_eq!(
            buttons.last().unwrap().x + buttons.last().unwrap().width,
            1000.0
        );
        assert!(buttons.windows(2).all(|pair| pair[0].x + 40.0 == pair[1].x));
        assert!((centered_text_left(buttons[0], "×", 16.0) - 815.2).abs() < 0.01);
    }

    #[test]
    fn centered_truncated_text_left_honors_bounds_and_keeps_ellipsis() {
        let bounds = Bounds {
            x: 100.0,
            y: 12.0,
            width: 44.0,
            height: 20.0,
        };
        let (truncated, left) = centered_truncated_text_left(bounds, "Trust Rule", 12.0);
        assert!(truncated.ends_with('…'));
        assert!(display_width(&truncated) <= 6);
        assert!((left - bounds.x).abs() < 20.0);
    }

    #[test]
    fn titlebar_sidebar_is_left_while_window_controls_stay_right() {
        let buttons = titlebar_chrome_layout(1000.0);
        assert_eq!(buttons[0].x, 0.0);
        assert_eq!(buttons[1].x, 840.0);
        assert_eq!(buttons.last().unwrap().x + 40.0, 1000.0);
    }

    #[test]
    fn wheel_selection_clamps_at_both_ends() {
        assert_eq!(scroll_selection(2, 5, -3), 1);
        assert_eq!(scroll_selection(2, 5, 3), 3);
        assert_eq!(scroll_selection(0, 5, -1), 0);
        assert_eq!(scroll_selection(4, 5, 1), 4);
        assert_eq!(scroll_selection(8, 0, 1), 0);
    }

    #[test]
    fn list_scrollbar_tracks_the_visible_slice() {
        let panel = Bounds {
            x: 10.0,
            y: 20.0,
            width: 200.0,
            height: 120.0,
        };
        let (track, first) = list_scrollbar(panel, 20, 5, 0).unwrap();
        let (_, last) = list_scrollbar(panel, 20, 5, 15).unwrap();
        assert_eq!(first.y, track.y);
        assert_eq!(last.y + last.height, track.y + track.height);
        assert!(list_scrollbar(panel, 5, 5, 0).is_none());
    }

    #[test]
    fn panels_stay_inside_small_and_large_windows() {
        for (width, height) in [(320.0, 240.0), (600.0, 360.0), (1920.0, 1080.0)] {
            let panel = centered_panel(width, height, 520.0, 600.0, 16.0);
            assert!(panel.x >= 0.0 && panel.y >= 0.0);
            assert!(panel.x + panel.width <= width + f32::EPSILON);
            assert!(panel.y + panel.height <= height + f32::EPSILON);
        }
    }

    #[test]
    fn pointer_menu_clamps_and_hits_only_visible_rows() {
        let menu = popup_menu(590.0, 350.0, 600.0, 360.0, 210.0, 30.0, 3);
        assert!(menu.x >= 0.0 && menu.y >= 0.0);
        assert!(menu.x + menu.width <= 600.0);
        assert!(menu.y + menu.height <= 360.0);
        assert_eq!(
            menu_row_at(menu, menu.x + 4.0, menu.y + 5.0, 30.0, 3),
            Some(0)
        );
        assert_eq!(
            menu_row_at(menu, menu.x + 4.0, menu.y + 65.0, 30.0, 3),
            Some(2)
        );
        assert_eq!(menu_row_at(menu, menu.x - 1.0, menu.y + 5.0, 30.0, 3), None);
    }

    #[test]
    fn tab_overflow_menu_keeps_long_titles_when_the_window_has_room() {
        let lines =
            vec!["  1  a-long-latin-title-with-more-than-twenty-eight-columns 中文标题".into()];
        let layout = tab_overflow_menu_layout(20.0, 30.0, 1000.0, 600.0, &lines, 0);
        assert_eq!(layout.visible, 0..1);
        assert_eq!(layout.line(&lines, 0), lines[0]);
        assert!(layout.panel.width > 240.0);
    }

    #[test]
    fn tab_overflow_menu_truncates_unicode_to_the_final_row_bounds() {
        let lines = vec!["✓ 1  很长很长的中文标题-and-a-long-latin-tail".into()];
        let layout = tab_overflow_menu_layout(20.0, 30.0, 150.0, 200.0, &lines, 0);
        let rendered = layout.line(&lines, 0);
        assert!(UnicodeWidthStr::width(rendered.as_str()) <= layout.text_columns);
        assert!(rendered.ends_with('…'));
        assert!(layout.panel.x + layout.panel.width <= 150.0);
    }

    #[test]
    fn tab_overflow_menu_scrolls_to_and_hits_the_last_surface() {
        let lines = (0..20)
            .map(|index| format!("  {}  surface", index + 1))
            .collect::<Vec<_>>();
        let layout = tab_overflow_menu_layout(20.0, 30.0, 500.0, 130.0, &lines, usize::MAX);
        assert_eq!(layout.visible.end, 20);
        let row = menu_row_at(
            layout.panel,
            layout.panel.x + 4.0,
            layout.panel.y + (layout.visible.len() - 1) as f32 * TAB_OVERFLOW_ROW_HEIGHT + 5.0,
            TAB_OVERFLOW_ROW_HEIGHT,
            layout.visible.len(),
        )
        .unwrap();
        assert_eq!(layout.surface_index(row), Some(19));
    }

    #[test]
    fn tab_overflow_menu_range_maps_draw_hover_and_click_indices_consistently() {
        let lines = (0..12)
            .map(|index| format!("  {}  surface", index + 1))
            .collect::<Vec<_>>();
        let layout = tab_overflow_menu_layout(20.0, 30.0, 500.0, 130.0, &lines, 4);
        let drawn = layout.visible.clone().collect::<Vec<_>>();
        let hovered = (0..layout.visible.len())
            .map(|row| layout.surface_index(row).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(drawn, hovered);
    }

    #[test]
    fn tab_overflow_scroll_offset_clamps_and_can_reveal_active_surface() {
        assert_eq!(tab_overflow_scroll_offset(20, 3, 0, -99), 0);
        assert_eq!(tab_overflow_scroll_offset(20, 3, 0, 99), 17);
        assert_eq!(tab_overflow_initial_offset(20, 3, 19), 17);
        assert_eq!(tab_overflow_initial_offset(20, 3, 1), 0);
    }

    #[test]
    fn target_hit_uses_pointer_bounds_instead_of_active_index() {
        let panes = [
            Bounds {
                x: 10.0,
                y: 40.0,
                width: 200.0,
                height: 300.0,
            },
            Bounds {
                x: 216.0,
                y: 40.0,
                width: 200.0,
                height: 300.0,
            },
        ];
        assert_eq!(bounds_at(&panes, 300.0, 100.0), Some(1));
        assert_eq!(bounds_at(&panes, 212.0, 100.0), None);
    }

    #[test]
    fn scrolled_ranges_clamp_at_the_end() {
        assert_eq!(scrolled_range(20, 0, 5), 0..5);
        assert_eq!(scrolled_range(20, 8, 5), 8..13);
        assert_eq!(scrolled_range(20, 99, 5), 15..20);
        assert_eq!(scrolled_range(0, 5, 5), 0..0);
    }

    #[test]
    fn palette_highlights_align_with_rendered_action_lines() {
        let panel = Bounds {
            x: 100.0,
            y: 50.0,
            width: 456.0,
            height: 376.0,
        };
        let rows = palette_rows(panel, 12);
        assert_eq!(rows[0].y, 58.0);
        assert_eq!(rows[1].y, 83.0);
        assert!(rows.iter().all(|row| row.x >= panel.x));
        assert!(
            rows.iter()
                .all(|row| row.y + row.height <= panel.y + panel.height)
        );
    }

    #[test]
    fn palette_hover_hits_rows_but_not_header_or_gaps_outside_panel() {
        let panel = Bounds {
            x: 100.0,
            y: 80.0,
            width: 456.0,
            height: 200.0,
        };
        let rows = palette_rows(panel, 3);
        assert_eq!(
            palette_row_at(panel, 3, rows[1].x + 4.0, rows[1].y + 4.0),
            Some(1)
        );
        assert_eq!(
            palette_row_at(panel, 3, panel.x + 20.0, panel.y + 10.0),
            Some(0)
        );
        assert_eq!(palette_row_at(panel, 3, panel.x - 1.0, rows[0].y), None);
    }

    #[test]
    fn hit_testing_rejects_rows_that_are_not_rendered() {
        assert_eq!(fixed_row_at(100.0, 100.0, 63.0, 2), Some(0));
        assert_eq!(fixed_row_at(226.0, 100.0, 63.0, 2), None);
        assert_eq!(fixed_row_at(99.0, 100.0, 63.0, 2), None);
    }

    #[test]
    fn notification_rows_follow_panel_and_text_origin() {
        let panel = Bounds {
            x: 250.0,
            y: 48.0,
            width: 350.0,
            height: 510.0,
        };
        let rows = notification_rows(panel, 20);
        assert_eq!(rows[0].x, panel.x + 8.0);
        assert_eq!(rows[0].y, panel.y + 16.0 + 2.0 * 21.0);
        assert_eq!(rows[1].y, rows[0].y + 96.0);
        assert!(
            rows.last().unwrap().y + rows.last().unwrap().height <= panel.y + panel.height - 8.0
        );
    }

    #[test]
    fn mark_all_read_button_stays_in_notification_header() {
        let panel = Bounds {
            x: 100.0,
            y: 50.0,
            width: 350.0,
            height: 500.0,
        };
        let button = notification_mark_all_read_button(panel);
        assert!(panel.contains(button.x, button.y));
        assert!(panel.contains(
            button.x + button.width - f32::EPSILON,
            button.y + button.height - f32::EPSILON
        ));
        let first_row = notification_row_layouts(panel, 1)[0].bounds;
        assert!(button.y + button.height <= first_row.y);
        assert!(notification_mark_all_read_hit(
            panel,
            true,
            button.x + 1.0,
            button.y + 1.0
        ));
        assert!(!notification_mark_all_read_hit(
            panel,
            false,
            button.x + 1.0,
            button.y + 1.0
        ));
    }

    #[test]
    fn notification_row_hit_uses_the_same_geometry_as_drawing() {
        let panel = Bounds {
            x: 250.0,
            y: 48.0,
            width: 350.0,
            height: 510.0,
        };
        let rows = notification_row_layouts(panel, 20);
        let first = rows[0];
        assert_eq!(
            notification_row_at(panel, 20, first.close.x + 2.0, first.close.y + 2.0),
            Some(NotificationRowHit::Close(0))
        );
        assert_eq!(
            notification_row_at(panel, 20, first.content.x + 2.0, first.content.y + 2.0),
            Some(NotificationRowHit::Open(0))
        );
        assert_eq!(
            notification_row_at(panel, 20, panel.x + 2.0, first.bounds.y + 2.0),
            None
        );
    }

    #[test]
    fn agent_approval_buttons_share_draw_and_hit_geometry() {
        let region = Bounds {
            x: 620.0,
            y: 90.0,
            width: 360.0,
            height: 260.0,
        };
        let cards = agent_approval_cards(region, &[2, 4, 3]);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].buttons.len(), 2);
        assert_eq!(cards[1].buttons.len(), 4);
        let target = cards[1].buttons[3];
        assert_eq!(
            agent_approval_hit(region, &[2, 4, 3], target.x + 1.0, target.y + 1.0),
            Some(AgentApprovalHit { card: 1, action: 3 })
        );
        assert_eq!(
            agent_approval_hit(region, &[2, 4, 3], cards[0].content.x, cards[0].content.y),
            None
        );
    }

    #[test]
    fn truncation_preserves_unicode_boundaries() {
        assert_eq!(truncate("这是一个很长的工作区名称", 6), "这是一个很…");
        assert_eq!(truncate("short", 8), "short");
        assert_eq!(truncate_display_width("中文 workspace", 8), "中文 wo…");
        assert_eq!(
            UnicodeWidthStr::width(truncate_display_width("中文名", 5).as_str()),
            5
        );
        assert_eq!(
            truncate_display_width_start("/very/long/project/path", 12),
            "…roject/path"
        );
    }

    #[test]
    fn agent_spinner_uses_a_stable_slot_without_hiding_the_title() {
        assert_eq!(
            surface_tab_title("⠋ compiling wmcterm workspace", true),
            "⠋ compiling wmcterm workspace"
        );
        assert_eq!(surface_tab_title("⠙ 中文终端标题", true), "⠙ 中文终端标题");
        assert_eq!(surface_tab_title("wmcterm", true), "  wmcterm");
        assert_eq!(surface_tab_title("⠋braille-title", false), "⠋braille-title");
        assert_eq!(
            surface_tab_label("⠹ fixing tests", 2, true),
            "⠹ fixing tests 3"
        );
        assert_eq!(
            surface_tab_width_at("wmcterm", 0, 8.0, true),
            surface_tab_width_at("⠼ wmcterm", 0, 8.0, true)
        );
        let width = surface_tab_width_at("⠼ wmcterm", 0, 7.2, true);
        let tab = Bounds {
            x: 0.0,
            y: 0.0,
            width,
            height: 25.0,
        };
        let close = Bounds {
            x: width - SURFACE_TAB_MAX_CLOSE_WIDTH,
            y: 0.0,
            width: SURFACE_TAB_MAX_CLOSE_WIDTH,
            height: 25.0,
        };
        let (title, _) = surface_tab_text_bounds(tab, Some(close), 0, 7.2);
        assert!(title.width >= display_width(&surface_tab_title("⠼ wmcterm", true)) as f32 * 7.2);
    }

    #[test]
    fn notification_preview_wraps_two_lines_and_marks_hidden_tail() {
        assert_eq!(
            notification_preview("一二三四五六七八九十", 6, 2, None),
            vec!["一二三".to_owned(), "四五…".to_owned()]
        );
        assert_eq!(
            notification_preview("alpha beta gamma", 6, 2, Some(1)),
            vec!["beta g".to_owned(), "amma".to_owned()]
        );
    }

    #[test]
    fn bounds_use_half_open_hit_regions() {
        let bounds = Bounds {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 40.0,
        };
        assert!(bounds.contains(10.0, 20.0));
        assert!(!bounds.contains(40.0, 60.0));
    }

    #[test]
    fn tab_label_geometry_tracks_full_titles_wide_characters_and_font_size() {
        let long = surface_tab_width_at(
            "a title that is much longer than the visible tab",
            0,
            8.0,
            false,
        );
        let equally_visible = surface_tab_width_at("a title that …", 0, 8.0, false);
        assert!(long > equally_visible);
        assert_eq!(surface_tab_label("terminal", 0, false), "terminal 1");
        assert_eq!(surface_tab_width_at("tab", 0, 8.0, false), 75.0);
        assert_eq!(
            surface_tab_width_at("终端", 0, 8.0, false),
            surface_tab_width_at("term", 0, 8.0, false)
        );
        assert!(
            surface_tab_width_at("tab", 0, 12.0, false)
                > surface_tab_width_at("tab", 0, 8.0, false)
        );
        assert!(
            surface_tab_width_at("tab", 9, 8.0, false) > surface_tab_width_at("tab", 0, 8.0, false)
        );
    }

    #[test]
    fn squeezed_tab_title_is_truncated_before_word_wrapping() {
        assert_eq!(
            surface_tab_visible_title("terminal workspace", false, 8.0 * 8.0, 8.0),
            "termina…"
        );
        assert_eq!(
            display_width(&surface_tab_visible_title(
                "中文 terminal",
                false,
                6.0 * 8.0,
                8.0,
            )),
            6
        );
        assert_eq!(
            surface_tab_visible_title("terminal workspace", false, 24.0 * 8.0, 8.0),
            "terminal workspace"
        );
    }

    #[test]
    fn tab_titles_discard_trailing_terminal_padding() {
        assert_eq!(
            surface_tab_title("Claude Code        ", false),
            "Claude Code"
        );
        assert_eq!(
            surface_tab_title("⠙ fixing tests\t \n", true),
            "⠙ fixing tests"
        );
        assert_eq!(
            surface_tab_title("Claude Code        ", true),
            "  Claude Code"
        );
        assert_eq!(
            surface_tab_visible_title("Claude Code        ", false, 8.0 * 8.0, 8.0),
            "Claude …"
        );
    }

    #[test]
    fn tab_numbers_keep_independent_space_before_the_close_target() {
        for index in [7, 8, 9, 10] {
            let width = surface_tab_width_at("long terminal title", index, 7.2, false);
            let tab = Bounds {
                x: 100.0,
                y: 40.0,
                width,
                height: 25.0,
            };
            let close = Bounds {
                x: tab.x + tab.width - 24.0,
                y: tab.y,
                width: 24.0,
                height: tab.height,
            };
            let (title, number) = surface_tab_text_bounds(tab, Some(close), index, 7.2);
            assert!(title.x + title.width <= number.x);
            assert!(number.width >= display_width(&(index + 1).to_string()) as f32 * 7.2);
            assert!(number.x + number.width < close.x);
        }
    }

    #[test]
    fn sidebar_scroll_clamps_and_uses_the_same_row_rects_for_hits() {
        let layout = sidebar_layout(240.0, 320.0, &[46.0; 10], 99);
        assert_eq!(layout.scroll, layout.max_scroll);
        assert_eq!(layout.rows.last().unwrap().index, 9);
        for row in &layout.rows {
            assert!(row.bounds.contains(20.0, row.bounds.y + 1.0));
        }
        assert!(
            layout
                .rows
                .windows(2)
                .all(|rows| rows[0].bounds.y + rows[0].bounds.height == rows[1].bounds.y)
        );
        assert_eq!(layout.new_workspace.y, 49.0);
        assert_eq!(layout.new_workspace.x, 200.0);
    }

    #[test]
    fn sidebar_layout_stacks_variable_height_rows_without_hit_gaps() {
        let layout = sidebar_layout(240.0, 400.0, &[64.0, 82.0, 64.0], 0);
        assert_eq!(layout.rows.len(), 3);
        assert_eq!(layout.rows[0].bounds.height, 64.0);
        assert_eq!(layout.rows[1].bounds.height, 82.0);
        assert!(
            layout
                .rows
                .windows(2)
                .all(|rows| { rows[0].bounds.y + rows[0].bounds.height == rows[1].bounds.y })
        );
    }

    #[test]
    fn sidebar_layout_reserves_a_context_menu_target_below_rows() {
        let height = 400.0;
        let layout = sidebar_layout(240.0, height, &[64.0; 20], 0);
        let rows_bottom = layout
            .rows
            .last()
            .map_or(88.0, |row| row.bounds.y + row.bounds.height);
        assert!(height - rows_bottom >= 32.0);
        assert!(
            !layout
                .rows
                .iter()
                .any(|row| row.bounds.contains(20.0, height - 1.0))
        );
    }

    #[test]
    fn workspace_drag_target_uses_row_midpoints() {
        let layout = sidebar_layout(240.0, 400.0, &[64.0, 82.0, 64.0], 0);

        assert_eq!(workspace_drag_target(&layout.rows, 0, 200.0), 1);
        assert_eq!(workspace_drag_target(&layout.rows, 0, 280.0), 2);
        assert_eq!(workspace_drag_target(&layout.rows, 2, 90.0), 0);
    }

    #[test]
    fn workspace_drag_target_stays_put_before_crossing_a_midpoint() {
        let layout = sidebar_layout(240.0, 400.0, &[64.0, 82.0, 64.0], 0);

        assert_eq!(workspace_drag_target(&layout.rows, 1, 125.0), 1);
        assert_eq!(workspace_drag_target(&layout.rows, 1, 169.0), 1);
    }

    #[test]
    fn sidebar_layout_does_not_let_a_row_invade_a_small_footer() {
        let height = 100.0;
        let layout = sidebar_layout(240.0, height, &[64.0], 0);
        assert!(layout.rows.is_empty());
        assert!(
            !layout
                .rows
                .iter()
                .any(|row| row.bounds.contains(20.0, height - 1.0))
        );
    }

    #[test]
    fn active_surface_tab_is_always_visible_and_close_targets_do_not_overlap() {
        let pane = Bounds {
            x: 100.0,
            y: 40.0,
            width: 250.0,
            height: 300.0,
        };
        for active in 0..8 {
            let tabs = surface_tabs_layout(pane, 8, active);
            assert!(tabs.iter().any(|tab| tab.index == active));
            assert!(
                tabs.windows(2)
                    .all(|tabs| { tabs[0].bounds.x + tabs[0].bounds.width <= tabs[1].bounds.x })
            );
            assert!(tabs.iter().all(|tab| {
                tab.close.is_some_and(|close| {
                    tab.bounds.contains(close.x, close.y)
                        && close.x + close.width <= tab.bounds.x + tab.bounds.width
                })
            }));
        }
    }

    #[test]
    fn surface_tabs_share_spare_width_up_to_a_comfortable_maximum() {
        let tabs = surface_tabs_layout(
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 500.0,
                height: 300.0,
            },
            2,
            0,
        );

        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs[0].bounds.width, 236.0);
        assert_eq!(tabs[1].bounds.width, 236.0);
    }

    #[test]
    fn hidden_surface_tabs_reserve_a_visible_overflow_target() {
        let tabs = surface_tabs_layout(
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 250.0,
                height: 300.0,
            },
            8,
            4,
        );

        let tabs_right = tabs.last().unwrap().bounds.x + tabs.last().unwrap().bounds.width;
        let overflow = surface_tab_overflow_bounds(
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 250.0,
                height: 300.0,
            },
            tabs.len(),
            8,
        )
        .unwrap();
        assert_eq!(overflow.x, 178.0);
        assert_eq!(overflow.width, 44.0);
        assert!(tabs_right <= overflow.x);
        assert!(tabs.iter().any(|tab| tab.index == 4));
    }

    #[test]
    fn switching_within_a_visible_tab_page_keeps_positions_stable() {
        let pane = Bounds {
            x: 0.0,
            y: 0.0,
            width: 250.0,
            height: 300.0,
        };
        let before = surface_tabs_layout(pane, 8, 2);
        let after = surface_tabs_layout(pane, 8, 3);

        assert_eq!(before, after);
        assert_eq!(
            before.iter().map(|tab| tab.index).collect::<Vec<_>>(),
            [2, 3]
        );
    }

    #[test]
    fn final_surface_can_be_closed_to_leave_an_empty_pane() {
        let tabs = surface_tabs_layout(
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 200.0,
            },
            1,
            0,
        );
        assert_eq!(tabs.len(), 1);
        assert!(tabs[0].close.is_some());
    }

    #[test]
    fn scrollbar_endpoints_map_to_full_scrollback() {
        let layout = terminal_scrollbar_layout(
            Bounds {
                x: 10.0,
                y: 40.0,
                width: 500.0,
                height: 400.0,
            },
            20,
            180,
            0,
        )
        .unwrap();
        assert_eq!(
            layout.thumb.y + layout.thumb.height,
            layout.track.y + layout.track.height
        );
        assert_eq!(layout.offset_for_pointer(layout.track.y, 0.0), 180);
        assert_eq!(
            layout.offset_for_pointer(layout.track.y + layout.track.height, layout.thumb.height),
            0
        );
    }

    #[test]
    fn scrollbar_track_clicks_page_toward_the_pointer() {
        let layout = terminal_scrollbar_layout(
            Bounds {
                x: 0.0,
                y: 0.0,
                width: 300.0,
                height: 300.0,
            },
            24,
            200,
            100,
        )
        .unwrap();
        assert_eq!(layout.page_delta_at(layout.thumb.y - 1.0), Some(-24));
        assert_eq!(
            layout.page_delta_at(layout.thumb.y + layout.thumb.height + 1.0),
            Some(24)
        );
        assert_eq!(layout.page_delta_at(layout.thumb.y + 1.0), None);
    }

    #[test]
    fn scrollbar_is_absent_without_history() {
        assert!(
            terminal_scrollbar_layout(
                Bounds {
                    x: 0.0,
                    y: 0.0,
                    width: 300.0,
                    height: 300.0
                },
                24,
                0,
                0,
            )
            .is_none()
        );
    }
}
