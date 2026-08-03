use std::collections::HashSet;
use std::io::{self, Stdout};

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};
use unicode_width::UnicodeWidthStr;

const BACKGROUND: Color = Color::Rgb(0x15, 0x15, 0x15);
const FOREGROUND: Color = Color::Rgb(0xe8, 0xe8, 0xd3);
const ACCENT: Color = Color::Rgb(0x8f, 0xbf, 0xdc);
const SELECTION: Color = Color::Rgb(0x40, 0x40, 0x40);
const MUTED: Color = Color::Rgb(0x60, 0x59, 0x58);
const DISABLED: Color = Color::Rgb(0x88, 0x88, 0x88);
const RED: Color = Color::Rgb(0xd7, 0x45, 0x45);
const GREEN: Color = Color::Rgb(0x99, 0xad, 0x6a);
const YELLOW: Color = Color::Rgb(0xfa, 0xd0, 0x7a);
const TEAL: Color = Color::Rgb(0x66, 0x87, 0x99);
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

pub struct Choice<T> {
    pub value: T,
    pub title: String,
    pub detail: Option<String>,
    pub search_text: String,
    parent: Option<usize>,
    tree_root: bool,
    status: Option<ChoiceStatus>,
    current: bool,
    alternate_order: usize,
    primary_only: bool,
    alternate_only: bool,
    context: Option<String>,
    inline_detail: bool,
    detail_primary_only: bool,
    prioritize_alternate_order: bool,
    primary_suffix: Option<String>,
    highlighted: bool,
    preserve_primary_order_in_search: bool,
}

impl<T> Choice<T> {
    pub fn new(
        value: T,
        title: impl Into<String>,
        detail: Option<impl Into<String>>,
        search_text: impl Into<String>,
    ) -> Self {
        Self {
            value,
            title: title.into(),
            detail: detail.map(Into::into),
            search_text: search_text.into(),
            parent: None,
            tree_root: false,
            status: None,
            current: false,
            alternate_order: usize::MAX,
            primary_only: false,
            alternate_only: false,
            context: None,
            inline_detail: false,
            detail_primary_only: false,
            prioritize_alternate_order: false,
            primary_suffix: None,
            highlighted: false,
            preserve_primary_order_in_search: false,
        }
    }

    pub fn child_of(mut self, parent: usize) -> Self {
        self.parent = Some(parent);
        self
    }

    pub fn tree_root(mut self) -> Self {
        self.tree_root = true;
        self
    }

    pub fn with_optional_status(mut self, status: Option<ChoiceStatus>) -> Self {
        self.status = status;
        self
    }

    pub fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }

    pub fn alternate_order(mut self, order: usize) -> Self {
        self.alternate_order = order;
        self
    }

    pub fn primary_only(mut self) -> Self {
        self.primary_only = true;
        self
    }

    pub fn alternate_only(mut self) -> Self {
        self.alternate_only = true;
        self
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    pub fn inline_detail(mut self, primary_only: bool) -> Self {
        self.inline_detail = true;
        self.detail_primary_only = primary_only;
        self
    }

    pub fn prioritize_alternate_order(mut self) -> Self {
        self.prioritize_alternate_order = true;
        self
    }

    pub fn with_primary_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.primary_suffix = Some(suffix.into());
        self
    }

    pub fn highlighted(mut self, highlighted: bool) -> Self {
        self.highlighted = highlighted;
        self
    }

    pub fn preserve_primary_order_in_search(mut self) -> Self {
        self.preserve_primary_order_in_search = true;
        self
    }
}

pub struct Picker<'a> {
    pub placeholder: &'a str,
    pub empty_message: &'a str,
    pub order: Option<OrderToggle<'a>>,
}

#[derive(Clone, Copy)]
pub struct OrderToggle<'a> {
    pub primary: &'a str,
    pub alternate: &'a str,
    pub initial_alternate: bool,
    pub kind: ToggleKind,
}

/// Decides which side of the tab row a toggle belongs to: what the picker
/// lists on the left, how the picker ranks rows on the right.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToggleKind {
    View,
    Sort,
}

struct PickerState {
    query: String,
    cursor: usize,
    matches: Vec<usize>,
    selected: usize,
    tick: usize,
    alternate_order: bool,
    priority_sort: bool,
}

impl PickerState {
    #[cfg(test)]
    fn new<T>(choices: &[Choice<T>]) -> Self {
        Self::new_with_order(choices, false)
    }

    fn new_with_order<T>(choices: &[Choice<T>], alternate_order: bool) -> Self {
        let selected = choices
            .iter()
            .rposition(|choice| choice.current)
            .unwrap_or(0);
        Self {
            query: String::new(),
            cursor: 0,
            matches: (0..choices.len()).collect(),
            selected,
            tick: 0,
            alternate_order,
            priority_sort: true,
        }
    }

    fn update_matches<T>(&mut self, choices: &[Choice<T>]) {
        let active = choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice_visible(choice, self.alternate_order))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if active
            .iter()
            .any(|index| choices[*index].tree_root || choices[*index].parent.is_some())
        {
            self.matches =
                hierarchical_matches(choices, &active, &self.query, self.alternate_order);
        } else if self.query.is_empty() {
            self.matches = active;
            if self.alternate_order {
                self.matches
                    .sort_by_key(|index| choices[*index].alternate_order);
            }
        } else {
            let mut matches = active
                .into_iter()
                .filter_map(|index| {
                    let choice = &choices[index];
                    fuzzy_score(&choice.search_text, &self.query).map(|score| (index, score))
                })
                .collect::<Vec<_>>();
            let prioritize_alternate = self.alternate_order
                && matches
                    .iter()
                    .any(|(index, _)| choices[*index].prioritize_alternate_order);
            let preserve_primary = !self.alternate_order
                && matches
                    .iter()
                    .any(|(index, _)| choices[*index].preserve_primary_order_in_search);
            matches.sort_by(|(left_index, left_score), (right_index, right_score)| {
                let priority = if preserve_primary {
                    left_index.cmp(right_index)
                } else if prioritize_alternate && self.priority_sort {
                    choices[*left_index]
                        .alternate_order
                        .cmp(&choices[*right_index].alternate_order)
                } else {
                    std::cmp::Ordering::Equal
                };
                priority
                    .then_with(|| right_score.cmp(left_score))
                    .then_with(|| left_index.cmp(right_index))
            });
            self.matches = matches.into_iter().map(|(index, _)| index).collect();
        }
        self.selected = 0;
    }

    fn insert(&mut self, character: char) {
        self.query.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.query[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.query[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn backspace(&mut self) {
        let previous = self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index);
        if let Some(previous) = previous {
            self.query.drain(previous..self.cursor);
            self.cursor = previous;
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.query[self.cursor..].chars().next() {
            self.query
                .drain(self.cursor..self.cursor + character.len_utf8());
        }
    }

    fn delete_previous_word(&mut self) {
        let mut start = self.cursor;
        while let Some((index, character)) = self.query[..start].char_indices().next_back() {
            if !character.is_whitespace() {
                break;
            }
            start = index;
        }
        while let Some((index, character)) = self.query[..start].char_indices().next_back() {
            if character.is_whitespace() {
                break;
            }
            start = index;
        }
        self.query.drain(start..self.cursor);
        self.cursor = start;
    }

    fn delete_to_start(&mut self) {
        self.query.drain(..self.cursor);
        self.cursor = 0;
    }

    fn set_alternate_order<T>(&mut self, alternate: bool, choices: &[Choice<T>]) {
        self.alternate_order = alternate;
        self.update_matches(choices);
        if alternate {
            self.selected = 0;
        } else {
            self.select_current(choices);
        }
    }

    fn toggle_order<T>(&mut self, choices: &[Choice<T>]) {
        self.set_alternate_order(!self.alternate_order, choices);
    }

    fn select_current<T>(&mut self, choices: &[Choice<T>]) {
        if self.query.is_empty() {
            if let Some(position) = self
                .matches
                .iter()
                .rposition(|index| choices[*index].current)
            {
                self.selected = position;
            }
        }
    }

    fn move_up(&mut self) {
        if !self.matches.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.matches.len() - 1);
        }
    }

    fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }
}

enum InputOutcome {
    Continue,
    Select(usize),
    Cancel,
}

pub fn pick<T>(picker: Picker<'_>, choices: Vec<Choice<T>>) -> Result<Option<T>, String> {
    pick_inner(picker, choices, |_| None).map(|(selection, _)| selection)
}

pub fn pick_with_detail<T, F>(
    picker: Picker<'_>,
    choices: Vec<Choice<T>>,
    detail_loader: F,
) -> Result<(Option<T>, bool), String>
where
    F: FnMut(&T) -> Option<String>,
{
    pick_inner(picker, choices, detail_loader)
}

fn pick_inner<T, F>(
    picker: Picker<'_>,
    mut choices: Vec<Choice<T>>,
    mut detail_loader: F,
) -> Result<(Option<T>, bool), String>
where
    F: FnMut(&T) -> Option<String>,
{
    let mut session = TerminalSession::start()?;
    let mut state = PickerState::new_with_order(
        &choices,
        picker.order.is_some_and(|order| order.initial_alternate),
    );
    state.update_matches(&choices);
    if !state.alternate_order {
        state.select_current(&choices);
    }
    let mut details_loaded = HashSet::new();

    let outcome = loop {
        if let Some(index) = state.matches.get(state.selected).copied() {
            if details_loaded.insert(index) {
                if let Some(detail) = detail_loader(&choices[index].value) {
                    choices[index].detail = Some(detail);
                }
            }
        }
        session
            .terminal
            .draw(|frame| {
                render(
                    frame,
                    picker.placeholder,
                    picker.empty_message,
                    picker.order,
                    &choices,
                    &state,
                )
            })
            .map_err(|error| format!("failed to draw picker: {error}"))?;

        if !event::poll(std::time::Duration::from_millis(80))
            .map_err(|error| format!("failed to poll picker input: {error}"))?
        {
            state.tick = state.tick.wrapping_add(1);
            continue;
        }
        let event =
            event::read().map_err(|error| format!("failed to read picker input: {error}"))?;
        match handle_event(event, &mut state, &choices, picker.order.is_some()) {
            InputOutcome::Continue => {}
            InputOutcome::Select(index) => break Some(index),
            InputOutcome::Cancel => break None,
        }
    };

    session.restore()?;
    let alternate_order = state.alternate_order;
    let selection = outcome.map(|index| {
        choices
            .into_iter()
            .nth(index)
            .expect("selected index is valid")
            .value
    });
    Ok((selection, alternate_order))
}

fn handle_event<T>(
    event: Event,
    state: &mut PickerState,
    choices: &[Choice<T>],
    has_order_toggle: bool,
) -> InputOutcome {
    let Event::Key(key) = event else {
        return InputOutcome::Continue;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return InputOutcome::Continue;
    }

    match key {
        KeyEvent {
            code: KeyCode::Esc, ..
        }
        | KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => InputOutcome::Cancel,
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } => state
            .matches
            .get(state.selected)
            .copied()
            .map(InputOutcome::Select)
            .unwrap_or(InputOutcome::Continue),
        KeyEvent {
            code: KeyCode::Up, ..
        }
        | KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.move_up();
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Down,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.move_down();
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Backspace,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('h'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.backspace();
            state.update_matches(choices);
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Delete,
            ..
        } => {
            state.delete();
            state.update_matches(choices);
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Left,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('b'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.move_left();
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Right,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('f'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.move_right();
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Home,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.cursor = 0;
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::End, ..
        }
        | KeyEvent {
            code: KeyCode::Char('e'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.cursor = state.query.len();
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.delete_previous_word();
            state.update_matches(choices);
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            ..
        } if has_order_toggle => {
            state.toggle_order(choices);
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if priority_sort_applies(choices, state.alternate_order) => {
            state.priority_sort = !state.priority_sort;
            state.update_matches(choices);
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => {
            state.delete_to_start();
            state.update_matches(choices);
            InputOutcome::Continue
        }
        KeyEvent {
            code: KeyCode::Char(character),
            modifiers,
            ..
        } if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            state.insert(character);
            state.update_matches(choices);
            InputOutcome::Continue
        }
        _ => InputOutcome::Continue,
    }
}

fn render<T>(
    frame: &mut Frame,
    placeholder: &str,
    empty_message: &str,
    order: Option<OrderToggle<'_>>,
    choices: &[Choice<T>],
    state: &PickerState,
) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BACKGROUND)),
        area,
    );

    // Reserve the tab row for every view of a picker that has tabs, so
    // toggling views never shifts the input or the results.
    let tab_row = order.is_some() || has_priority_sort(choices);
    let mut constraints = Vec::with_capacity(4);
    if tab_row {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Length(1));
    constraints.push(Constraint::Min(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut index = 0;
    if tab_row {
        render_tabs(frame, chunks[index], order, choices, state);
        index += 1;
    }
    render_query(frame, chunks[index], placeholder, state);
    render_results(frame, chunks[index + 2], empty_message, choices, state);
}

fn render_tabs<T>(
    frame: &mut Frame,
    area: Rect,
    order: Option<OrderToggle<'_>>,
    choices: &[Choice<T>],
    state: &PickerState,
) {
    let order_tabs = order.map(|order| (order.kind, order_tabs(order, state.alternate_order)));
    let left = order_tabs
        .as_ref()
        .filter(|(kind, _)| *kind == ToggleKind::View)
        .map(|(_, tabs)| tabs.clone());
    // No picker pairs a sort toggle with priority sorting; the sort toggle wins
    // the right slot when one ever does.
    let right = order_tabs
        .as_ref()
        .filter(|(kind, _)| *kind == ToggleKind::Sort)
        .map(|(_, tabs)| tabs.clone())
        .or_else(|| {
            priority_sort_applies(choices, state.alternate_order)
                .then(|| mode_tabs(state.priority_sort))
        });

    let left_width = left.as_deref().map(span_width).unwrap_or(0);
    if let Some(tabs) = left.filter(|_| area.width >= left_width) {
        render_tab_group(frame, Rect::new(area.x, area.y, left_width, 1), tabs);
    }
    let Some(tabs) = right else {
        return;
    };
    let width = span_width(&tabs);
    // Drop the sort tabs rather than let them collide with the view tabs.
    if width == 0 || area.width < left_width + width + 2 {
        return;
    }
    render_tab_group(
        frame,
        Rect::new(area.x + area.width - width, area.y, width, 1),
        tabs,
    );
}

fn render_tab_group(frame: &mut Frame, area: Rect, tabs: Vec<Span<'_>>) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(tabs)).style(Style::default().bg(BACKGROUND)),
        area,
    );
}

fn span_width(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>() as u16
}

fn render_query(frame: &mut Frame, area: Rect, placeholder: &str, state: &PickerState) {
    let content = if state.query.is_empty() {
        Line::from(vec![
            Span::styled(
                "› ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(placeholder, Style::default().fg(MUTED)),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "› ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(&state.query, Style::default().fg(FOREGROUND)),
        ])
    };
    frame.render_widget(
        Paragraph::new(content).style(Style::default().bg(BACKGROUND)),
        area,
    );
    if area.width > 0 && area.height > 0 {
        let query_column = UnicodeWidthStr::width(&state.query[..state.cursor]) as u16;
        let x = area
            .x
            .saturating_add(2)
            .saturating_add(query_column)
            .min(area.x + area.width.saturating_sub(1));
        frame.set_cursor_position((x, area.y));
    }
}

fn order_tabs<'a>(order: OrderToggle<'a>, alternate: bool) -> Vec<Span<'a>> {
    let active_style = Style::default()
        .fg(BACKGROUND)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(MUTED).bg(BACKGROUND);
    vec![
        Span::styled(
            format!(" {} ", order.primary),
            if alternate {
                inactive_style
            } else {
                active_style
            },
        ),
        Span::styled(" ", Style::default().bg(BACKGROUND)),
        Span::styled(
            format!(" {} ", order.alternate),
            if alternate {
                active_style
            } else {
                inactive_style
            },
        ),
    ]
}

fn mode_tabs(priority: bool) -> Vec<Span<'static>> {
    let active_style = Style::default()
        .fg(BACKGROUND)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let disabled_style = Style::default().fg(DISABLED).bg(BACKGROUND);
    vec![
        Span::styled(
            " prio ",
            if priority {
                active_style
            } else {
                disabled_style
            },
        ),
        Span::styled(" ", Style::default().bg(BACKGROUND)),
        Span::styled(
            " fuzzy ",
            if priority {
                disabled_style
            } else {
                active_style
            },
        ),
    ]
}

/// Whether a result row gets a second line for its detail text. Shared by
/// render_results's per-item ListItem construction and by item_height below,
/// so the overflow badges can never drift out of sync with what's actually
/// drawn.
fn has_detail_line<T>(choice: &Choice<T>, show_details: bool) -> bool {
    show_details && !choice.inline_detail && choice.detail.is_some()
}

fn item_height<T>(choice: &Choice<T>, show_details: bool) -> u16 {
    if has_detail_line(choice, show_details) {
        2
    } else {
        1
    }
}

/// Mirrors ratatui's `List::get_items_bounds` for a `ListState` that starts
/// at offset 0 every frame (how render_results always builds it) with no
/// scroll padding. `ListState` only exposes the resulting offset (the first
/// visible position) after rendering, not the last one, so the overflow
/// badges need their own copy of this math to know exactly what got clipped.
fn visible_match_range(heights: &[u16], selected: usize, max_height: u16) -> (usize, usize) {
    if heights.is_empty() {
        return (0, 0);
    }
    let selected = selected.min(heights.len() - 1);
    let max_height = u32::from(max_height);
    let mut first = 0usize;
    let mut last = 0usize;
    let mut height = 0u32;
    for next_height in heights {
        if height + u32::from(*next_height) > max_height {
            break;
        }
        height += u32::from(*next_height);
        last += 1;
    }
    while selected >= last {
        height += u32::from(heights[last]);
        last += 1;
        while height > max_height {
            height -= u32::from(heights[first]);
            first += 1;
        }
    }
    (first, last)
}

/// Corner overflow badges, echoing the scroll indicator in this user's pi
/// editor (`\u2500\u2500\u2500 \u2191 N more \u2500\u2500\u2500`) but as a small overlay instead of a dedicated
/// border row, since the results area has none to spare.
fn render_overflow_badges(
    frame: &mut Frame,
    area: Rect,
    content_height: u16,
    items_above: usize,
    items_below: usize,
) {
    if content_height == 0 || area.width < 24 {
        return;
    }
    // A single visible row can't hold both badges without them colliding;
    // keep the bottom one since scrolling further down is the more common
    // next move.
    let show_above = items_above > 0 && !(content_height <= 1 && items_below > 0);
    if show_above {
        render_overflow_badge(frame, area.x, area.y, area.width, "\u{2191}", items_above);
    }
    if items_below > 0 {
        render_overflow_badge(
            frame,
            area.x,
            area.y + content_height - 1,
            area.width,
            "\u{2193}",
            items_below,
        );
    }
}

fn render_overflow_badge(
    frame: &mut Frame,
    x: u16,
    y: u16,
    max_width: u16,
    arrow: &str,
    count: usize,
) {
    let text = format!(" {arrow} {count} more ");
    let width = UnicodeWidthStr::width(text.as_str()) as u16;
    if width == 0 || width > max_width || width > max_width / 2 {
        return;
    }
    frame.render_widget(
        Paragraph::new(text).style(
            Style::default()
                .fg(BACKGROUND)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Rect::new(x + max_width - width, y, width, 1),
    );
}

fn render_results<T>(
    frame: &mut Frame,
    area: Rect,
    empty_message: &str,
    choices: &[Choice<T>],
    state: &PickerState,
) {
    if state.matches.is_empty() {
        frame.render_widget(
            Paragraph::new(empty_message).style(Style::default().fg(MUTED).bg(BACKGROUND)),
            area,
        );
        return;
    }

    let hierarchical = state
        .matches
        .iter()
        .any(|index| choices[*index].tree_root || choices[*index].parent.is_some());
    let show_details = !hierarchical && area.height >= 14;
    let items = state.matches.iter().enumerate().map(|(position, index)| {
        let choice = &choices[*index];
        let selected = position == state.selected;
        let title_style = if selected {
            Style::default().fg(FOREGROUND).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(FOREGROUND)
        };
        let marker = if selected { "› " } else { "  " };
        let tree = if hierarchical {
            tree_prefix(*index, choices, &state.matches)
        } else {
            ""
        };
        let working = choice.status == Some(ChoiceStatus::Working);
        let (gutter, gutter_color) = if choice.current {
            ("◆", ACCENT)
        } else if choice.highlighted {
            ("◆", TEAL)
        } else if working {
            (working_indicator(state.tick), YELLOW)
        } else {
            (" ", ACCENT)
        };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(ACCENT)),
            Span::styled(format!("{gutter} "), Style::default().fg(gutter_color)),
            Span::styled(tree, Style::default().fg(MUTED)),
        ];
        if let Some(context) = &choice.context {
            spans.push(Span::styled(
                format!("{context} "),
                Style::default().fg(if selected { FOREGROUND } else { ACCENT }),
            ));
        }
        spans.push(Span::styled(&choice.title, title_style));
        if let Some(status) = choice.status {
            let (icon, color) = status_icon(status);
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{icon} {}", status_label(status)),
                Style::default().fg(color),
            ));
        }
        let show_inline_detail = hierarchical
            || (choice.inline_detail && (!choice.detail_primary_only || !state.alternate_order));
        if show_inline_detail {
            if let Some(detail) = &choice.detail {
                spans.push(Span::styled(
                    format!("  {detail}"),
                    Style::default().fg(if selected { FOREGROUND } else { MUTED }),
                ));
            }
        }
        if !state.alternate_order {
            if let Some(suffix) = &choice.primary_suffix {
                spans.push(Span::styled(
                    format!("  {suffix}"),
                    Style::default().fg(MUTED),
                ));
            }
        }
        let mut lines = vec![Line::from(spans)];
        if has_detail_line(choice, show_details) {
            if let Some(detail) = &choice.detail {
                let detail_color = if selected { FOREGROUND } else { MUTED };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(detail, Style::default().fg(detail_color)),
                ]));
            }
        }
        ListItem::new(lines)
    });
    let list = List::new(items)
        .style(Style::default().bg(BACKGROUND))
        .highlight_style(Style::default().bg(SELECTION));
    let mut list_state = ListState::default().with_selected(Some(state.selected));
    frame.render_stateful_widget(list, area, &mut list_state);

    let heights = state
        .matches
        .iter()
        .map(|index| item_height(&choices[*index], show_details))
        .collect::<Vec<_>>();
    let (first_visible, last_visible) = visible_match_range(&heights, state.selected, area.height);
    let content_height = heights[first_visible..last_visible].iter().sum();
    render_overflow_badges(
        frame,
        area,
        content_height,
        first_visible,
        heights.len() - last_visible,
    );
}

fn hierarchical_matches<T>(
    choices: &[Choice<T>],
    active: &[usize],
    query: &str,
    alternate_order: bool,
) -> Vec<usize> {
    let mut groups = Vec::new();
    for root_index in active
        .iter()
        .copied()
        .filter(|index| choices[*index].parent.is_none())
    {
        let root = &choices[root_index];
        let root_score = (!query.is_empty())
            .then(|| fuzzy_score(&root.search_text, query))
            .flatten();
        let root_matches = query.is_empty() || root_score.is_some();
        let children = active
            .iter()
            .copied()
            .filter(|index| choices[*index].parent == Some(root_index))
            .map(|index| (index, &choices[index]))
            .collect::<Vec<_>>();
        let scored_children = children
            .iter()
            .filter_map(|(index, child)| {
                if query.is_empty() {
                    Some((*index, 0))
                } else {
                    fuzzy_score(&child.search_text, query).map(|score| (*index, score))
                }
            })
            .collect::<Vec<_>>();
        if root_matches || !scored_children.is_empty() {
            let score = root_score
                .into_iter()
                .chain(scored_children.iter().map(|(_, score)| *score))
                .max()
                .unwrap_or(0);
            let mut group = vec![root_index];
            if root_matches {
                let mut child_indices = children
                    .into_iter()
                    .map(|(index, _)| index)
                    .collect::<Vec<_>>();
                if alternate_order && query.is_empty() {
                    child_indices.sort_by_key(|index| choices[*index].alternate_order);
                }
                group.extend(child_indices);
            } else {
                group.extend(scored_children.into_iter().map(|(index, _)| index));
            }
            groups.push((root_index, score, group));
        }
    }
    if !query.is_empty() {
        groups.sort_by(
            |(left_index, left_score, _), (right_index, right_score, _)| {
                right_score
                    .cmp(left_score)
                    .then_with(|| left_index.cmp(right_index))
            },
        );
    } else if alternate_order {
        groups.sort_by_key(|(root_index, _, _)| choices[*root_index].alternate_order);
    }
    groups.into_iter().flat_map(|(_, _, group)| group).collect()
}

fn choice_visible<T>(choice: &Choice<T>, alternate: bool) -> bool {
    if alternate {
        !choice.primary_only
    } else {
        !choice.alternate_only
    }
}

fn priority_sort_applies<T>(choices: &[Choice<T>], alternate: bool) -> bool {
    alternate && has_priority_sort(choices)
}

fn has_priority_sort<T>(choices: &[Choice<T>]) -> bool {
    choices
        .iter()
        .any(|choice| choice.prioritize_alternate_order)
}

fn tree_prefix<T>(index: usize, choices: &[Choice<T>], visible: &[usize]) -> &'static str {
    let Some(parent) = choices[index].parent else {
        return if choices.iter().any(|choice| choice.parent == Some(index)) {
            "▾ "
        } else {
            "  "
        };
    };
    let has_later_sibling = visible
        .iter()
        .skip_while(|visible_index| **visible_index != index)
        .skip(1)
        .any(|visible_index| choices[*visible_index].parent == Some(parent));
    if has_later_sibling {
        "├── "
    } else {
        "└── "
    }
}

fn status_icon(status: ChoiceStatus) -> (&'static str, Color) {
    match status {
        ChoiceStatus::Blocked => ("◉", RED),
        ChoiceStatus::Working => ("●", YELLOW),
        ChoiceStatus::Done => ("●", TEAL),
        ChoiceStatus::Idle => ("✓", GREEN),
        ChoiceStatus::Unknown => ("○", MUTED),
    }
}

fn working_indicator(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

fn status_label(status: ChoiceStatus) -> &'static str {
    match status {
        ChoiceStatus::Blocked => "blocked",
        ChoiceStatus::Working => "working",
        ChoiceStatus::Done => "done",
        ChoiceStatus::Idle => "idle",
        ChoiceStatus::Unknown => "unknown",
    }
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    let mut score = 0_i64;
    let mut previous_match = None;

    for (index, character) in candidate.chars().enumerate() {
        if character != wanted {
            continue;
        }
        score += 10;
        if previous_match == Some(index.saturating_sub(1)) {
            score += 8;
        }
        if index == 0
            || candidate
                .chars()
                .nth(index.saturating_sub(1))
                .is_some_and(|previous| matches!(previous, '/' | '-' | '_' | ' ' | '.'))
        {
            score += 12;
        }
        previous_match = Some(index);
        match query_chars.next() {
            Some(next) => wanted = next,
            None => return Some(score - index as i64),
        }
    }
    None
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    restored: bool,
}

impl TerminalSession {
    fn start() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| format!("failed to enable raw mode: {error}"))?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Show) {
            let _ = execute!(stdout, Show, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(format!("failed to enter picker screen: {error}"));
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                return Err(format!("failed to initialize picker: {error}"));
            }
        };
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        let screen_result = execute!(self.terminal.backend_mut(), Show, LeaveAlternateScreen);
        let raw_result = disable_raw_mode();
        self.restored = true;
        screen_result.map_err(|error| format!("failed to leave picker screen: {error}"))?;
        raw_result.map_err(|error| format!("failed to disable raw mode: {error}"))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<Choice<&'static str>> {
        vec![
            Choice::new(
                "flip",
                "Flip split direction",
                Some("Two-pane tabs"),
                "flip split direction",
            ),
            Choice::new(
                "move",
                "Move pane to new workspace",
                Some("Detach the focused pane"),
                "move pane to new workspace",
            ),
        ]
    }

    #[test]
    fn fuzzy_match_rewards_consecutive_and_word_boundary_matches() {
        let strong = fuzzy_score("flip split direction", "fs").unwrap();
        let weak = fuzzy_score("office status", "fs").unwrap();
        assert!(strong > weak);
    }

    #[test]
    fn filtering_is_ranked_and_stable() {
        let choices = choices();
        let mut state = PickerState::new(&choices);
        state.query = "move".into();
        state.update_matches(&choices);
        assert_eq!(state.matches, vec![1]);

        state.query.clear();
        state.update_matches(&choices);
        assert_eq!(state.matches, vec![0, 1]);
    }

    #[test]
    fn navigation_wraps_without_underflowing_empty_results() {
        let choices = choices();
        let mut state = PickerState::new(&choices);
        state.move_up();
        assert_eq!(state.selected, 1);
        state.move_down();
        assert_eq!(state.selected, 0);

        state.matches.clear();
        state.move_up();
        state.move_down();
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn readline_editing_preserves_utf8_boundaries() {
        let choices = choices();
        let mut state = PickerState::new(&choices);
        for character in "café au lait".chars() {
            state.insert(character);
        }
        state.delete_previous_word();
        assert_eq!(state.query, "café au ");
        assert_eq!(state.cursor, state.query.len());

        state.move_left();
        state.move_left();
        state.insert('X');
        assert_eq!(state.query, "café aXu ");
        state.delete_to_start();
        assert_eq!(state.query, "u ");
        assert_eq!(state.cursor, 0);
        state.delete();
        assert_eq!(state.query, " ");
    }

    #[test]
    fn tree_filter_cascades_parent_matches_and_keeps_matching_context() {
        let choices = vec![
            Choice::new("one", "Cast", None::<String>, "cast workspace").tree_root(),
            Choice::new("one-a", "shell", None::<String>, "shell").child_of(0),
            Choice::new("one-b", "Build agent", None::<String>, "build agent").child_of(0),
            Choice::new("two", "Docs", None::<String>, "docs workspace").tree_root(),
            Choice::new("two-a", "Writer", None::<String>, "writer agent").child_of(3),
        ];
        let active = (0..choices.len()).collect::<Vec<_>>();
        assert_eq!(
            hierarchical_matches(&choices, &active, "cast", false),
            vec![0, 1, 2]
        );
        assert_eq!(
            hierarchical_matches(&choices, &active, "writer", false),
            vec![3, 4]
        );
        assert_eq!(
            hierarchical_matches(&choices, &active, "", false),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn tree_filter_ranks_the_best_matching_workspace_group_first() {
        let choices = vec![
            Choice::new("one", "Archive", None::<String>, "archive herdr").tree_root(),
            Choice::new("one-a", "shell", None::<String>, "notes").child_of(0),
            Choice::new("two", "herdr-cast", None::<String>, "herdr cast").tree_root(),
            Choice::new("two-a", "pi", None::<String>, "pi agent").child_of(2),
        ];
        let active = (0..choices.len()).collect::<Vec<_>>();
        assert_eq!(
            hierarchical_matches(&choices, &active, "herdr", false),
            vec![2, 3, 0, 1]
        );
    }

    #[test]
    fn alternate_order_changes_the_empty_query_base_order() {
        let choices = vec![
            Choice::new("z", "zeta", None::<String>, "zeta").alternate_order(1),
            Choice::new("a", "alpha", None::<String>, "alpha").alternate_order(0),
        ];
        let mut state = PickerState::new(&choices);
        state.set_alternate_order(true, &choices);
        assert_eq!(state.matches, vec![1, 0]);
        state.set_alternate_order(false, &choices);
        assert_eq!(state.matches, vec![0, 1]);
    }

    #[test]
    fn alternate_view_selects_first_match_and_primary_view_selects_current() {
        let choices = vec![
            Choice::new("first", "first", None::<String>, "first"),
            Choice::new("current", "current", None::<String>, "current").current(true),
        ];
        let mut state = PickerState::new(&choices);
        state.update_matches(&choices);
        state.select_current(&choices);
        assert_eq!(state.selected, 1);

        state.set_alternate_order(true, &choices);
        assert_eq!(state.selected, 0);

        state.set_alternate_order(false, &choices);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn alternate_priority_precedes_fuzzy_score_when_requested() {
        let choices = vec![
            Choice::new("idle", "cast", None::<String>, "cast")
                .alternate_order(3)
                .prioritize_alternate_order(),
            Choice::new("done", "archive cast", None::<String>, "archive cast")
                .alternate_order(1)
                .prioritize_alternate_order(),
        ];
        let mut state = PickerState::new_with_order(&choices, true);
        state.query = "cast".into();
        state.cursor = state.query.len();
        state.update_matches(&choices);
        assert_eq!(state.matches, vec![1, 0]);
    }

    #[test]
    fn priority_sort_toggle_lets_fuzzy_score_beat_status_order() {
        let choices = vec![
            Choice::new("done", "archive cast", None::<String>, "archive cast")
                .alternate_order(1)
                .prioritize_alternate_order(),
            Choice::new("idle", "cast", None::<String>, "cast")
                .alternate_order(3)
                .prioritize_alternate_order(),
        ];
        let mut state = PickerState::new_with_order(&choices, true);
        state.query = "cast".into();
        state.cursor = state.query.len();
        // Default keeps today's behaviour: agent status (done) outranks fuzzy score.
        state.update_matches(&choices);
        assert_eq!(state.matches, vec![0, 1]);
        // Toggle to fuzzy ranking: the exact match surfaces above the done agent.
        state.priority_sort = false;
        state.update_matches(&choices);
        assert_eq!(state.matches, vec![1, 0]);
    }

    #[test]
    fn primary_order_can_be_preserved_while_filtering() {
        let choices = vec![
            Choice::new("ranked", "archive cast", None::<String>, "archive cast")
                .preserve_primary_order_in_search(),
            Choice::new("fuzzy", "cast", None::<String>, "cast").preserve_primary_order_in_search(),
        ];
        let mut state = PickerState::new(&choices);
        state.query = "cast".into();
        state.cursor = state.query.len();
        state.update_matches(&choices);
        assert_eq!(state.matches, vec![0, 1]);

        state.set_alternate_order(true, &choices);
        assert_eq!(state.matches, vec![1, 0]);
    }

    #[test]
    fn visible_match_range_shows_as_many_uniform_rows_as_fit_from_the_top() {
        let heights = vec![1; 10];
        assert_eq!(visible_match_range(&heights, 0, 4), (0, 4));
    }

    #[test]
    fn visible_match_range_scrolls_minimally_to_keep_the_selection_as_the_last_row() {
        let heights = vec![1; 10];
        assert_eq!(visible_match_range(&heights, 5, 4), (2, 6));
        assert_eq!(visible_match_range(&heights, 9, 4), (6, 10));
    }

    #[test]
    fn visible_match_range_accounts_for_two_line_items_when_trimming_the_front() {
        // Heights: [2, 1, 1, 2, 1], budget 3.
        let heights = vec![2, 1, 1, 2, 1];
        assert_eq!(visible_match_range(&heights, 0, 3), (0, 2));
        assert_eq!(visible_match_range(&heights, 4, 3), (3, 5));
    }

    #[test]
    fn visible_match_range_never_panics_when_a_single_item_exceeds_the_budget() {
        assert_eq!(visible_match_range(&[5], 0, 0), (1, 1));
        assert_eq!(visible_match_range(&[1, 1, 1], 2, 0), (3, 3));
    }

    #[test]
    fn visible_match_range_treats_an_empty_list_as_fully_visible() {
        assert_eq!(visible_match_range(&[], 0, 10), (0, 0));
    }
}

#[cfg(test)]
mod layout {
    use super::*;
    use ratatui::backend::TestBackend;

    fn rows<T>(
        choices: &[Choice<T>],
        order: Option<OrderToggle<'_>>,
        alternate: bool,
    ) -> Vec<String> {
        rows_sized(choices, order, alternate, 60, 12)
    }

    fn rows_sized<T>(
        choices: &[Choice<T>],
        order: Option<OrderToggle<'_>>,
        alternate: bool,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let mut state = PickerState::new_with_order(choices, alternate);
        state.update_matches(choices);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    "Search workspaces and panes",
                    "No matches",
                    order,
                    choices,
                    &state,
                )
            })
            .unwrap();
        rows_from_buffer(terminal.backend().buffer())
    }

    fn flat_choices(count: usize) -> Vec<Choice<usize>> {
        (0..count)
            .map(|index| {
                Choice::new(
                    index,
                    format!("item {index}"),
                    None::<String>,
                    format!("item {index}"),
                )
            })
            .collect()
    }

    /// Choices with a detail line, so at area.height >= 14 render_results
    /// gives every row a second line (see has_detail_line).
    fn detailed_choices(count: usize) -> Vec<Choice<usize>> {
        (0..count)
            .map(|index| {
                Choice::new(
                    index,
                    format!("item {index}"),
                    Some(format!("detail {index}")),
                    format!("item {index}"),
                )
            })
            .collect()
    }

    /// Renders only the results list (bypassing tabs/query) so tests can pin
    /// down an exact results-area height and selection for overflow-badge
    /// coverage.
    fn result_rows<T>(
        choices: &[Choice<T>],
        selected: usize,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let mut state = PickerState::new(choices);
        state.update_matches(choices);
        state.selected = selected;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_results(frame, frame.area(), "No matches", choices, &state))
            .unwrap();
        rows_from_buffer(terminal.backend().buffer())
    }

    fn rows_from_buffer(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn view_toggle() -> Option<OrderToggle<'static>> {
        Some(OrderToggle {
            primary: "spaces",
            alternate: "agents",
            initial_alternate: false,
            kind: ToggleKind::View,
        })
    }

    fn agent_choices() -> Vec<Choice<&'static str>> {
        vec![
            Choice::new("workspace", "herdr-cast", None::<String>, "herdr-cast").tree_root(),
            Choice::new("pane", "agent", None::<String>, "agent")
                .child_of(0)
                .alternate_only()
                .prioritize_alternate_order(),
        ]
    }

    fn query_row(rendered: &[String]) -> Option<usize> {
        rendered.iter().position(|row| row.starts_with('\u{203a}'))
    }

    #[test]
    fn view_tabs_lead_the_row_and_sort_tabs_trail_it() {
        let choices = agent_choices();
        let rendered = rows(&choices, view_toggle(), true);
        assert!(
            rendered[0].starts_with(" spaces ") && rendered[0].ends_with("fuzzy"),
            "unexpected tab row: {:?}",
            rendered[0]
        );
        // The query row now holds nothing but the prompt.
        assert_eq!(rendered[1], "\u{203a} Search workspaces and panes");
    }

    #[test]
    fn sort_toggles_stay_on_the_right_without_a_view_toggle() {
        let choices = vec![Choice::new(
            "dir",
            "aliou/herdr-cast",
            None::<String>,
            "herdr-cast",
        )];
        let rendered = rows(
            &choices,
            Some(OrderToggle {
                primary: "zoxide",
                alternate: "alpha",
                initial_alternate: false,
                kind: ToggleKind::Sort,
            }),
            false,
        );
        assert!(
            rendered[0].starts_with(' ') && rendered[0].ends_with("alpha"),
            "unexpected tab row: {:?}",
            rendered[0]
        );
        assert_eq!(query_row(&rendered), Some(1));
    }

    #[test]
    fn tab_row_is_reserved_so_views_share_a_layout() {
        let choices = agent_choices();
        let primary = rows(&choices, view_toggle(), false);
        let alternate = rows(&choices, view_toggle(), true);
        assert!(!primary[0].contains("prio"));
        assert!(primary[0].starts_with(" spaces ") && primary[0].contains("agents"));
        assert_eq!(query_row(&primary), query_row(&alternate));
    }

    #[test]
    fn narrow_pickers_drop_the_sort_tabs_before_the_view_tabs() {
        let choices = agent_choices();
        let rendered = rows_sized(&choices, view_toggle(), true, 20, 8);
        assert!(
            !rendered[0].contains("prio"),
            "unexpected sort tabs: {:?}",
            rendered[0]
        );
        assert!(
            rendered[0].contains("agents"),
            "missing view tabs: {:?}",
            rendered[0]
        );
    }

    #[test]
    fn pickers_without_tabs_keep_the_input_on_the_first_row() {
        let choices = vec![Choice::new(
            "flip",
            "Flip split direction",
            None::<String>,
            "flip",
        )];
        let rendered = rows(&choices, None, false);
        assert_eq!(query_row(&rendered), Some(0));
    }

    #[test]
    fn short_lists_show_no_overflow_badges() {
        let choices = flat_choices(5);
        let rendered = result_rows(&choices, 0, 40, 10);
        assert!(
            !rendered.iter().any(|row| row.contains("more")),
            "unexpected overflow badge: {rendered:?}"
        );
    }

    #[test]
    fn scrolled_to_the_top_shows_only_a_bottom_overflow_badge() {
        let choices = flat_choices(15);
        let rendered = result_rows(&choices, 0, 40, 5);
        assert!(
            !rendered[0].contains("more"),
            "unexpected top badge: {:?}",
            rendered[0]
        );
        assert!(
            rendered[4].contains("\u{2193} 10 more"),
            "missing bottom badge: {:?}",
            rendered[4]
        );
    }

    #[test]
    fn scrolled_into_the_middle_shows_both_overflow_badges() {
        let choices = flat_choices(15);
        let rendered = result_rows(&choices, 10, 40, 5);
        assert!(
            rendered[0].contains("\u{2191} 6 more"),
            "missing top badge: {:?}",
            rendered[0]
        );
        assert!(
            rendered[4].contains("\u{2193} 4 more"),
            "missing bottom badge: {:?}",
            rendered[4]
        );
    }

    #[test]
    fn scrolled_to_the_end_shows_only_a_top_overflow_badge() {
        let choices = flat_choices(15);
        let rendered = result_rows(&choices, 14, 40, 5);
        assert!(
            rendered[0].contains("\u{2191} 10 more"),
            "missing top badge: {:?}",
            rendered[0]
        );
        assert!(
            !rendered[4].contains("more"),
            "unexpected bottom badge: {:?}",
            rendered[4]
        );
    }

    #[test]
    fn overflow_badges_are_skipped_when_the_row_is_too_narrow() {
        let choices = flat_choices(15);
        let rendered = result_rows(&choices, 0, 20, 5);
        assert!(
            !rendered.iter().any(|row| row.contains("more")),
            "badge should not fit in 20 cols: {rendered:?}"
        );
    }

    #[test]
    fn two_line_detail_rows_are_accounted_for_in_the_overflow_count() {
        // Each row takes 2 lines once show_details kicks in at height >= 14
        // (has_detail_line), so exactly 7 of these 20 items fit. This guards
        // item_height and the real ListItem construction against drifting
        // apart \u2014 a naive 1-line-per-item assumption would claim 6 more
        // instead of 13.
        let choices = detailed_choices(20);
        let rendered = result_rows(&choices, 0, 40, 14);
        assert!(
            rendered[13].contains("\u{2193} 13 more"),
            "missing bottom badge accounting for 2-line rows: {:?}",
            rendered[13]
        );
    }

    #[test]
    fn a_single_visible_row_keeps_only_the_bottom_overflow_badge() {
        let choices = flat_choices(15);
        let rendered = result_rows(&choices, 7, 40, 1);
        assert!(
            !rendered[0].contains("\u{2191}"),
            "top badge should be suppressed: {:?}",
            rendered[0]
        );
        assert!(
            rendered[0].contains("\u{2193} 7 more"),
            "missing bottom badge: {:?}",
            rendered[0]
        );
    }
}
