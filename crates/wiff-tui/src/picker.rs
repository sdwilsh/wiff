//! A reusable modal list the reviewer scrolls and selects from.
//!
//! A [`Picker`] floats over the review, listing labeled rows one per line with
//! one highlighted. The highlight moves with the same navigation the main view
//! uses, and enter activates the highlighted row. What a row does on activation
//! is its own concern: each row is a [`PickerRow`] over the host it mutates, so
//! the widget itself knows nothing of files, comments, or themes and the same
//! modal drives all of them. A list taller than the space it is given scrolls,
//! keeping the highlighted row in view.
//!
//! A picker built [filterable](Picker::filterable) can also be narrowed by a
//! typed query, but only once the reviewer asks for it: the list starts in
//! navigation mode with the motion keys live, and `/` enters filter mode, where
//! a prompt line appears and typing narrows the rows to a fuzzy match ranked
//! best first with the matched characters picked out. Escape leaves filter mode
//! back to navigation; the motion keys never double as query text.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wiff_diff::Rgb;

use crate::render::color;
use crate::wrap::wrap_line;

/// One row of a picker: the text it shows and what selecting it does to the
/// host `Ctx`. The widget holds these as trait objects so a single picker can
/// list rows of any kind, each carrying its own activation.
pub trait PickerRow<Ctx> {
    /// The text shown for this row.
    fn label(&self) -> String;

    /// The row's text as styled fragments, painting parts of it in colors other
    /// than the picker's default or striking it through. Defaults to `None`, so
    /// a row shows its `label` in one plain color.
    fn styled(&self) -> Option<Vec<RowSpan>> {
        None
    }

    /// Extra text the fuzzy filter matches against but does not show, appended
    /// after `label` when a filterable picker ranks this row. It lets a row be
    /// found by content beyond its visible summary, such as a comment's later
    /// lines. Empty by default, so a row matches only what it shows.
    fn additional_match_text(&self) -> String {
        String::new()
    }

    /// Act on the host now that this row has been chosen.
    fn activate(self: Box<Self>, ctx: &mut Ctx);
}

/// A styled fragment of a picker row: its text, an optional foreground color
/// that overrides the picker's default text color, and whether it is struck
/// through.
#[derive(Clone)]
pub struct RowSpan {
    /// The fragment's text.
    pub text: String,
    /// Its foreground color, or `None` to take the picker's default text color.
    pub color: Option<Rgb>,
    /// Whether it is drawn struck through.
    pub strikethrough: bool,
}

/// The colors a picker paints with, taken from the theme by the host.
#[derive(Debug, Clone, Copy)]
pub struct PickerColors {
    /// The modal's border and title color.
    pub border: Rgb,
    /// The background filling the modal, so its text reads over the theme's own
    /// background rather than whatever the terminal shows through.
    pub background: Rgb,
    /// The background washed over the highlighted row.
    pub selected_bg: Rgb,
    /// The color of the row text.
    pub text: Rgb,
    /// The color painted over the characters of a row that the filter query
    /// matched, picking them out of the row text.
    pub match_highlight: Rgb,
    /// The color of the key hint along the bottom.
    pub hint: Rgb,
}

/// A row's text prepared for the fuzzy matcher: the visible label concatenated
/// with the row's hidden additional match text, and the number of characters in
/// the label prefix. Matched positions at or past that prefix fall in the hidden
/// text and are not painted. Built once when the picker is made filterable so a
/// keystroke re-runs only the matcher, not the per-row string building.
struct Haystack {
    text: String,
    label_len: usize,
}

impl Haystack {
    /// Prepare `row`'s haystack: its label followed by its additional match text,
    /// with the label's character count recorded for the paint cut.
    fn build<Ctx>(row: &dyn PickerRow<Ctx>) -> Self {
        let label = row.label();
        let label_len = label.chars().count();
        let text = format!("{label}{}", row.additional_match_text());
        Self { text, label_len }
    }
}

/// The fuzzy filter over a picker's rows: the typed query, whether the reviewer
/// has entered filter mode, the prepared per-row haystacks, the reusable matcher
/// engine, and the key hint shown while filtering.
struct Filter {
    query: String,
    /// Whether the reviewer has entered filter input mode with `/`. Until then
    /// the picker navigates with the motion keys and hides the prompt.
    active: bool,
    haystacks: Vec<Haystack>,
    matcher: Matcher,
    hint: String,
}

/// A row that passed the filter: its index into the picker's `rows` and the
/// label grapheme positions the query matched, sorted and deduplicated for
/// painting. Positions falling in a row's hidden additional match text are
/// dropped, since only the label is drawn.
struct RowMatch {
    row: usize,
    highlights: Vec<usize>,
}

/// A modal list of [`PickerRow`]s over a host `Ctx`, with one row highlighted
/// and a scroll window that keeps it in view.
pub struct Picker<Ctx> {
    title: String,
    rows: Vec<Box<dyn PickerRow<Ctx>>>,
    /// The rows visible under the current filter, best match first, each with
    /// the label positions its query matched. Every row in order when the picker
    /// does not filter or its query is empty.
    matches: Vec<RowMatch>,
    /// The highlighted row, as an index into `matches`.
    selected: usize,
    /// The first content line of the visible window. Content lines are the note
    /// block (`lead` of them) followed by one line per row, so the note scrolls
    /// with the rows rather than sitting fixed above them.
    top: usize,
    /// The number of content lines the window shows, set by the host each frame
    /// from the space the modal is given.
    height: usize,
    /// The number of leading content lines the note block occupies (its wrapped
    /// lines plus a spacer), zero when there is no note. Cached from the host's
    /// per-frame viewport update so the scroll math can place the highlighted
    /// row past the note.
    lead: usize,
    /// The key hint shown along the bottom, naming the reviewer's own bound
    /// keys, built by the host.
    hint: String,
    /// An advisory shown above the rows, painted in the border and title color.
    note: Option<String>,
    /// The live fuzzy filter, present when the picker was built filterable.
    filter: Option<Filter>,
    colors: PickerColors,
}

impl<Ctx> Picker<Ctx> {
    /// A picker titled `title` over `rows`, opening on the first row, drawn with
    /// `colors`, with `hint` named along the bottom.
    pub fn new(
        title: &str,
        rows: Vec<Box<dyn PickerRow<Ctx>>>,
        hint: &str,
        colors: PickerColors,
    ) -> Self {
        let mut picker = Self {
            title: title.to_string(),
            rows,
            matches: Vec::new(),
            selected: 0,
            top: 0,
            height: 0,
            lead: 0,
            hint: hint.to_string(),
            note: None,
            filter: None,
            colors,
        };
        picker.reset_matches_to_all();
        picker
    }

    /// Allow the reviewer to narrow this picker by typing, entered with `/`. The
    /// list still starts and stays in navigation mode until then; `filter_hint`
    /// names the keys shown along the bottom while filtering, and the navigation
    /// hint gains a note that `/` filters.
    pub fn filterable(mut self, filter_hint: &str) -> Self {
        let haystacks = self
            .rows
            .iter()
            .map(|row| Haystack::build(row.as_ref()))
            .collect();
        self.hint = format!("{}  / filter", self.hint);
        self.filter = Some(Filter {
            query: String::new(),
            active: false,
            haystacks,
            matcher: Matcher::new(Config::DEFAULT),
            hint: filter_hint.to_string(),
        });
        self
    }

    /// Whether this picker can be narrowed by a typed query, whether or not the
    /// reviewer has entered filter mode yet.
    pub fn is_filterable(&self) -> bool {
        self.filter.is_some()
    }

    /// Whether the reviewer is in filter mode, typing a query. When false the
    /// picker navigates with the motion keys and the prompt is hidden.
    pub fn filtering(&self) -> bool {
        self.filter.as_ref().is_some_and(|filter| filter.active)
    }

    /// Enter filter mode, revealing the prompt so typed characters narrow the
    /// rows. Does nothing for a non-filterable picker or when already filtering.
    pub fn begin_filter(&mut self) {
        if let Some(filter) = self.filter.as_mut() {
            filter.active = true;
        }
    }

    /// Leave filter mode, clearing the query and widening the list back to every
    /// row with the highlight on the first. Does nothing when not filtering.
    pub fn end_filter(&mut self) {
        if let Some(filter) = self.filter.as_mut() {
            filter.active = false;
            filter.query.clear();
        }
        self.reset_matches_to_all();
        self.selected = 0;
        self.scroll_into_view();
    }

    /// The current filter query, empty when nothing is typed or the picker is
    /// not in filter mode.
    pub fn query(&self) -> &str {
        self.filter
            .as_ref()
            .map_or("", |filter| filter.query.as_str())
    }

    /// Append `ch` to the filter query and renarrow the rows, moving the
    /// highlight to the best match. Does nothing unless the picker is filtering.
    pub fn push_query(&mut self, ch: char) {
        if let Some(filter) = self.filter.as_mut()
            && filter.active
        {
            filter.query.push(ch);
            self.refilter();
        }
    }

    /// Delete the last character of the filter query and renarrow. Does nothing
    /// when the query is already empty or the picker is not filtering.
    pub fn pop_query(&mut self) {
        if let Some(filter) = self.filter.as_mut()
            && filter.active
            && filter.query.pop().is_some()
        {
            self.refilter();
        }
    }

    /// Set `matches` to every row in its original order, unfiltered.
    fn reset_matches_to_all(&mut self) {
        self.matches = (0..self.rows.len())
            .map(|row| RowMatch {
                row,
                highlights: Vec::new(),
            })
            .collect();
    }

    /// Rebuild `matches` from the current filter query: every row in order for
    /// an empty query, otherwise the rows the query matches, ranked best first
    /// with their matched label positions. Resets the highlight to the top.
    fn refilter(&mut self) {
        let query = match &self.filter {
            Some(filter) if filter.active => filter.query.clone(),
            _ => return,
        };
        if query.is_empty() {
            self.reset_matches_to_all();
        } else {
            let pattern = Pattern::parse(&query, CaseMatching::Ignore, Normalization::Smart);
            let filter = self.filter.as_mut().expect("filter present");
            let matcher = &mut filter.matcher;
            let mut scratch = Vec::new();
            let mut positions = Vec::new();
            let mut scored: Vec<(u32, RowMatch)> = Vec::new();
            for (row, haystack) in filter.haystacks.iter().enumerate() {
                positions.clear();
                let utf32 = Utf32Str::new(&haystack.text, &mut scratch);
                if let Some(score) = pattern.indices(utf32, matcher, &mut positions) {
                    // Matched positions are reliable character offsets only for
                    // an ASCII haystack; for anything else nucleo's grapheme
                    // folding makes them index a different sequence than the one
                    // painted, so the row is matched and ranked but left
                    // unpainted rather than risk highlighting the wrong glyphs.
                    let highlights = if haystack.text.is_ascii() {
                        positions.sort_unstable();
                        positions.dedup();
                        positions
                            .iter()
                            .map(|&pos| pos as usize)
                            .take_while(|&pos| pos < haystack.label_len)
                            .collect()
                    } else {
                        Vec::new()
                    };
                    scored.push((score, RowMatch { row, highlights }));
                }
            }
            // Rank best score first; a stable sort keeps ties in original order.
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.matches = scored.into_iter().map(|(_, entry)| entry).collect();
        }
        self.selected = 0;
        self.scroll_into_view();
    }

    /// Show a single-line `note` above the rows to lead the reviewer with a
    /// piece of context before they choose a row. Assumes single-width
    /// characters, matching the widget's char-based width model.
    pub fn set_note(&mut self, note: impl Into<String>) {
        self.note = Some(note.into());
    }

    /// The number of lines the note occupies at content width `width`: the note
    /// wrapped to that width plus a blank separator line, or zero when there is
    /// no note.
    pub fn note_height(&self, width: usize) -> usize {
        match self.note {
            Some(_) => self.note_lines(width.saturating_sub(2)).len() + 1,
            None => 0,
        }
    }

    /// The note wrapped to `label_width` columns, each line indented past the
    /// marker column and painted in the border and title color over the modal
    /// background. Empty when there is no note.
    fn note_lines(&self, label_width: usize) -> Vec<Line<'static>> {
        let Some(note) = &self.note else {
            return Vec::new();
        };
        let style = Style::default()
            .fg(color(self.colors.border))
            .bg(color(self.colors.background));
        // The note is plain single-style text: wrap_line supplies the line
        // breaking, and every line is painted uniformly in the note color.
        wrap_line(&Line::from(note.clone()), label_width)
            .into_iter()
            .map(|line| {
                let text: String = line.spans.iter().flat_map(|s| s.content.chars()).collect();
                Line::from(Span::styled(format!("  {text:<label_width$}"), style))
            })
            .collect()
    }

    /// Move the highlight to the previous row, stopping at the first.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.scroll_into_view();
    }

    /// Move the highlight to the next row, stopping at the last.
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.last_row());
        self.scroll_into_view();
    }

    /// Move the highlight up by a window's worth of rows.
    pub fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(self.page());
        self.scroll_into_view();
    }

    /// Move the highlight down by a window's worth of rows.
    pub fn page_down(&mut self) {
        self.selected = (self.selected + self.page()).min(self.last_row());
        self.scroll_into_view();
    }

    /// Move the highlight to the first row.
    pub fn to_top(&mut self) {
        self.selected = 0;
        self.scroll_into_view();
    }

    /// Move the highlight to the last row.
    pub fn to_bottom(&mut self) {
        self.selected = self.last_row();
        self.scroll_into_view();
    }

    /// Open the highlight on row `index`, clamped to the list, so a picker can
    /// start on the reviewer's current choice rather than the first row.
    pub fn select(&mut self, index: usize) {
        self.selected = index.min(self.last_row());
        self.scroll_into_view();
    }

    /// Set the viewport to `height` content lines, `lead` of them the note block
    /// ahead of the rows, keeping the highlighted row in view. The host measures
    /// both each frame from the space the modal is given.
    pub fn set_viewport(&mut self, height: usize, lead: usize) {
        self.height = height;
        self.lead = lead;
        self.scroll_into_view();
    }

    /// Consume the picker, acting on the host with the highlighted row. Does
    /// nothing when the picker has no visible rows.
    pub fn activate_selected(self, ctx: &mut Ctx) {
        let Some(row) = self.matches.get(self.selected).map(|entry| entry.row) else {
            return;
        };
        if let Some(entry) = self.rows.into_iter().nth(row) {
            entry.activate(ctx);
        }
    }

    /// The number of rows visible under the current filter.
    pub fn list_len(&self) -> usize {
        self.matches.len()
    }

    /// The number of pinned lines the filter prompt occupies above the rows: one
    /// while filtering, none otherwise. The host adds it to the modal chrome so
    /// the prompt does not eat into the scrolling rows.
    pub fn prompt_height(&self) -> usize {
        usize::from(self.filtering())
    }

    /// The index of the highlighted row.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Returns the first visible content line of the scroll window.
    pub fn top(&self) -> usize {
        self.top
    }

    /// The modal's border and title color.
    pub fn border(&self) -> Rgb {
        self.colors.border
    }

    /// The heading for the modal's border.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The background filling the modal, for the host to paint the border row
    /// and the cells the content does not cover.
    pub fn background(&self) -> Rgb {
        self.colors.background
    }

    /// The width of the modal's content inside its border: two columns for the
    /// highlight marker, then the widest of the row labels, the hint, and the
    /// note, so a one-line note (such as a bound pull-request URL) shows without
    /// wrapping wherever the terminal is wide enough. The host clamps this to the
    /// terminal width, past which the note wraps.
    pub fn width(&self) -> usize {
        let widest = self
            .rows
            .iter()
            .map(|row| row.label().chars().count())
            .max()
            .unwrap_or(0);
        let note = self.note.as_deref().map_or(0, |note| {
            note.lines()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0)
        });
        let hint = self.hint.chars().count();
        // Both hints share the modal, so it must be wide enough for either.
        let filter_hint = self
            .filter
            .as_ref()
            .map_or(0, |filter| filter.hint.chars().count());
        2 + widest.max(hint).max(note).max(filter_hint)
    }

    /// The modal's content lines for a content width of `width`: the visible
    /// window over the note block and rows, each row marked and highlighted when
    /// it is the selection, then a spacer and the key hint pinned below. Labels
    /// and the hint are clipped to fit.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let label_width = width.saturating_sub(2);
        let background = color(self.colors.background);
        let text_color = color(self.colors.text);
        let highlight = color(self.colors.match_highlight);
        let mut lines: Vec<Line<'static>> = Vec::new();
        // While filtering, the prompt is pinned above the scrolling rows so the
        // typed query stays in view however far down the list is scrolled.
        if self.filtering() {
            lines.push(prompt_line(
                self.query(),
                label_width,
                text_color,
                background,
            ));
        }
        // The scrollable content: the note block (its wrapped lines then a
        // spacer) ahead of one line per visible row. The window slides over all
        // of it, so a note too tall for the modal scrolls with the rows rather
        // than pushing the spacer and hint off the bottom.
        let mut content: Vec<Line<'static>> = self.note_lines(label_width);
        if self.note.is_some() {
            content.push(Line::from(Span::styled(
                " ".repeat(width),
                Style::default().bg(background),
            )));
        }
        content.extend((0..self.matches.len()).map(|index| {
            let entry = &self.matches[index];
            let selected = index == self.selected;
            let marker = if selected { "> " } else { "  " };
            let row_bg = if selected {
                color(self.colors.selected_bg)
            } else {
                background
            };
            let row = &self.rows[entry.row];
            let style = RowStyle {
                width: label_width,
                text: text_color,
                highlight,
                background: row_bg,
            };
            style.render(marker, row.styled(), || row.label(), &entry.highlights)
        }));
        let end = (self.top + self.height).min(content.len());
        lines.extend(content[self.top.min(content.len())..end].to_vec());
        lines.push(Line::from(Span::styled(
            " ".repeat(width),
            Style::default().bg(background),
        )));
        // Filter mode names its own keys; navigation mode names the movement,
        // select, and cancel keys along with the `/` that enters filter mode.
        let hint = match &self.filter {
            Some(filter) if filter.active => filter.hint.as_str(),
            _ => self.hint.as_str(),
        };
        lines.push(Line::from(Span::styled(
            format!("  {:<label_width$}", clip(hint, label_width)),
            Style::default().fg(color(self.colors.hint)).bg(background),
        )));
        lines
    }

    /// The last addressable visible row, or zero when none are visible.
    fn last_row(&self) -> usize {
        self.matches.len().saturating_sub(1)
    }

    /// A window's worth of rows for a page move, at least one.
    fn page(&self) -> usize {
        self.height.max(1)
    }

    /// Slide the window so the highlighted row stays visible. The row sits at
    /// content line `lead + selected`, past the note block.
    fn scroll_into_view(&mut self) {
        if self.height == 0 {
            return;
        }
        let cursor = self.lead + self.selected;
        if cursor < self.top {
            self.top = cursor;
        }
        let bottom = cursor + 1;
        if bottom > self.top + self.height {
            self.top = bottom - self.height;
        }
        let content_len = self.lead + self.matches.len();
        let max_top = content_len.saturating_sub(self.height);
        self.top = self.top.min(max_top);
    }
}

/// Clip `text` to at most `width` columns.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// Render the pinned filter prompt: a leading marker, the typed `query`, and a
/// block cursor, padded to `label_width` so it fills the modal. A query wider
/// than the field scrolls to keep its cursor end in view rather than clipping
/// the newest characters off the right.
fn prompt_line(
    query: &str,
    label_width: usize,
    text_color: Color,
    background: Color,
) -> Line<'static> {
    let full = format!("{query}\u{2588}");
    let overflow = full.chars().count().saturating_sub(label_width);
    let field: String = full.chars().skip(overflow).collect();
    Line::from(Span::styled(
        format!("> {field:<label_width$}"),
        Style::default().fg(text_color).bg(background),
    ))
}

/// The colors and width one row is painted with: the content width it fills, the
/// default text color, the color matched characters are picked out in, and the
/// row's own background, which differs for the highlighted row.
struct RowStyle {
    width: usize,
    text: Color,
    highlight: Color,
    background: Color,
}

impl RowStyle {
    /// Render one row: its styled spans or plain label, with the filter's matched
    /// positions picked out. An unmatched row (empty `highlights`) keeps the
    /// plain single-span rendering so a picker with no query draws exactly as it
    /// did before filtering was added. `label` is read only when a plain row
    /// needs highlighting.
    fn render(
        &self,
        marker: &str,
        styled: Option<Vec<RowSpan>>,
        label: impl FnOnce() -> String,
        highlights: &[usize],
    ) -> Line<'static> {
        match styled {
            Some(spans) if highlights.is_empty() => self.styled(marker, &spans),
            Some(spans) => self.highlighted(marker, &spans, highlights),
            None if highlights.is_empty() => {
                let width = self.width;
                let text = format!("{marker}{:<width$}", clip(&label(), width));
                Line::from(Span::styled(
                    text,
                    Style::default().fg(self.text).bg(self.background),
                ))
            }
            None => {
                let spans = [RowSpan {
                    text: label(),
                    color: None,
                    strikethrough: false,
                }];
                self.highlighted(marker, &spans, highlights)
            }
        }
    }

    /// Render one row's `spans` with the filter's matched label positions painted
    /// in the highlight color: the selection `marker`, then the characters
    /// clipped to the content width and padded to fill it, adjacent characters of
    /// one style coalesced into a single span. `highlights` are sorted character
    /// positions, only ever populated for an ASCII label where character and
    /// match offsets agree.
    fn highlighted(&self, marker: &str, spans: &[RowSpan], highlights: &[usize]) -> Line<'static> {
        let plain = Style::default().fg(self.text).bg(self.background);
        let mut out = vec![Span::styled(marker.to_string(), plain)];
        let mut position = 0usize;
        let mut used = 0usize;
        let mut run = String::new();
        let mut run_style: Option<Style> = None;
        'outer: for span in spans {
            let base = span.color.map(color).unwrap_or(self.text);
            for ch in span.text.chars() {
                if used >= self.width {
                    break 'outer;
                }
                let matched = highlights.binary_search(&position).is_ok();
                let mut style = Style::default()
                    .fg(if matched { self.highlight } else { base })
                    .bg(self.background);
                if span.strikethrough {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
                if run_style != Some(style) {
                    if let Some(previous) = run_style {
                        out.push(Span::styled(std::mem::take(&mut run), previous));
                    }
                    run_style = Some(style);
                }
                run.push(ch);
                used += 1;
                position += 1;
            }
        }
        if let Some(previous) = run_style {
            out.push(Span::styled(run, previous));
        }
        if used < self.width {
            out.push(Span::styled(" ".repeat(self.width - used), plain));
        }
        Line::from(out)
    }

    /// Render one styled row: the selection `marker`, then the row's `spans`
    /// clipped to the content width and padded to fill it, each painted over the
    /// row background in its own color or the default text color when it names
    /// none.
    fn styled(&self, marker: &str, spans: &[RowSpan]) -> Line<'static> {
        let plain = Style::default().fg(self.text).bg(self.background);
        let mut out = vec![Span::styled(marker.to_string(), plain)];
        let mut used = 0;
        for span in spans {
            if used >= self.width {
                break;
            }
            let text: String = span.text.chars().take(self.width - used).collect();
            used += text.chars().count();
            let mut style = Style::default()
                .fg(span.color.map(color).unwrap_or(self.text))
                .bg(self.background);
            if span.strikethrough {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
            out.push(Span::styled(text, style));
        }
        if used < self.width {
            out.push(Span::styled(" ".repeat(self.width - used), plain));
        }
        Line::from(out)
    }
}

#[cfg(test)]
mod tests {
    use super::{Picker, PickerColors, PickerRow};
    use crate::render::testutil::dump;
    use wiff_diff::Rgb;

    /// A row naming itself, recording its index into the host when activated. Its
    /// `extra` is hidden text the filter matches against beyond the label.
    struct Row {
        label: String,
        extra: String,
        index: usize,
    }

    impl PickerRow<Vec<usize>> for Row {
        fn label(&self) -> String {
            self.label.clone()
        }

        fn additional_match_text(&self) -> String {
            self.extra.clone()
        }

        fn activate(self: Box<Self>, ctx: &mut Vec<usize>) {
            ctx.push(self.index);
        }
    }

    /// The picker colors used across the tests, distinct so each shows in a dump.
    fn colors() -> PickerColors {
        PickerColors {
            border: Rgb {
                r: 0x11,
                g: 0x11,
                b: 0x11,
            },
            background: Rgb {
                r: 0x55,
                g: 0x55,
                b: 0x55,
            },
            selected_bg: Rgb {
                r: 0x22,
                g: 0x22,
                b: 0x22,
            },
            text: Rgb {
                r: 0x33,
                g: 0x33,
                b: 0x33,
            },
            match_highlight: Rgb {
                r: 0x66,
                g: 0x66,
                b: 0x66,
            },
            hint: Rgb {
                r: 0x44,
                g: 0x44,
                b: 0x44,
            },
        }
    }

    /// A picker over `count` rows labeled `item N`, its window set to `height`.
    fn picker(count: usize, height: usize) -> Picker<Vec<usize>> {
        let rows: Vec<Box<dyn PickerRow<Vec<usize>>>> = (0..count)
            .map(|index| {
                Box::new(Row {
                    label: format!("item {index}"),
                    extra: String::new(),
                    index,
                }) as Box<dyn PickerRow<Vec<usize>>>
            })
            .collect();
        let mut picker = Picker::new("Files", rows, HINT, colors());
        picker.set_viewport(height, 0);
        picker
    }

    /// A filterable picker over `labels` (each paired with hidden `extra` match
    /// text), its window set tall enough to show them all plus the prompt.
    fn filter_picker(labels: &[(&str, &str)], height: usize) -> Picker<Vec<usize>> {
        let rows: Vec<Box<dyn PickerRow<Vec<usize>>>> = labels
            .iter()
            .enumerate()
            .map(|(index, (label, extra))| {
                Box::new(Row {
                    label: (*label).to_string(),
                    extra: (*extra).to_string(),
                    index,
                }) as Box<dyn PickerRow<Vec<usize>>>
            })
            .collect();
        let mut picker = Picker::new("Files", rows, HINT, colors()).filterable(FILTER_HINT);
        picker.set_viewport(height, 0);
        picker
    }

    /// Enter filter mode, then type each character of `query` into `picker`'s
    /// filter in turn.
    fn type_query(picker: &mut Picker<Vec<usize>>, query: &str) {
        picker.begin_filter();
        for ch in query.chars() {
            picker.push_query(ch);
        }
    }

    /// A fixed navigation hint for the tests, so the rendered width does not
    /// depend on the keymap the host would build it from.
    const HINT: &str = "up/down move   enter select   esc cancel";

    /// A fixed filter-mode hint for the tests.
    const FILTER_HINT: &str = "type to filter   esc navigate";

    #[test]
    fn the_first_row_opens_highlighted_and_the_hint_sits_below() {
        // A short list fits its window whole: the first row opens marked and
        // washed, the rest plain, then a spacer and the key hint.
        let picker = picker(3, 5);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#222222|->> item 0                                  \n",
            "<#333333|#555555|->  item 1                                  \n",
            "<#333333|#555555|->  item 2                                  \n",
            "<-|#555555|->                                          \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel\n",
        );
    }

    #[test]
    fn moving_past_the_window_scrolls_to_keep_the_highlight_in_view() {
        // Five rows in a window three tall. Stepping down to the fourth row
        // scrolls the window so it shows rows two through four with the fourth
        // highlighted; the hint stays anchored below.
        let mut picker = picker(5, 3);
        for _ in 0..3 {
            picker.select_next();
        }
        wince::assert_eq!(picker.selected(), 3);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->  item 1                                  \n",
            "<#333333|#555555|->  item 2                                  \n",
            "<#333333|#222222|->> item 3                                  \n",
            "<-|#555555|->                                          \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel\n",
        );
    }

    #[test]
    fn selecting_the_next_row_stops_at_the_last_and_bottom_jumps_there() {
        // Stepping down never runs off the end, and to_bottom lands on the last
        // row with the window scrolled to show it.
        let mut picker = picker(4, 2);
        for _ in 0..10 {
            picker.select_next();
        }
        wince::assert_eq!(picker.selected(), 3);
        picker.to_top();
        wince::assert_eq!(picker.selected(), 0);
        picker.to_bottom();
        wince::assert_eq!(picker.selected(), 3);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->  item 2                                  \n",
            "<#333333|#222222|->> item 3                                  \n",
            "<-|#555555|->                                          \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel\n",
        );
    }

    #[test]
    fn a_long_note_widens_the_box_to_show_on_one_line_in_the_border_color() {
        // A note wider than the rows and hint widens the modal so it shows on
        // one line, painted in the border and title color, then a blank line
        // before the rows.
        let mut picker = picker(2, 5);
        picker.set_note("the review base moved out from under you to a different commit");
        wince::assert_eq!(picker.note_height(picker.width()), 2);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#111111|#555555|->  the review base moved out from under you to a different commit\n",
            "<-|#555555|->                                                                \n",
            "<#333333|#222222|->> item 0                                                        \n",
            "<#333333|#555555|->  item 1                                                        \n",
            "<-|#555555|->                                                                \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel                      \n",
        );
    }

    #[test]
    fn a_note_wraps_when_clamped_narrower_than_it_leading_the_rows_in_the_border_color() {
        // Rendered to a width narrower than the note, as the host clamps to the
        // terminal, the note wraps across as many lines as it needs, breaking at
        // spaces, painted in the border and title color, then a blank line
        // before the rows.
        let mut picker = picker(2, 5);
        picker.set_note("the review base moved out from under you to a different commit");
        wince::assert_eq!(picker.note_height(40), 3);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(40)),
            "<#111111|#555555|->  the review base moved out from under  \n",
            "<#111111|#555555|->  you to a different commit             \n",
            "<-|#555555|->                                        \n",
            "<#333333|#222222|->> item 0                                \n",
            "<#333333|#555555|->  item 1                                \n",
            "<-|#555555|->                                        \n",
            "<#444444|#555555|->  up/down move   enter select   esc canc\n",
        );
    }

    #[test]
    fn a_note_scrolls_with_the_rows_when_the_content_overflows_the_viewport() {
        // Note plus rows exceed a three-line viewport. At the top the note shows
        // above the first row; stepping to the last row scrolls the note off,
        // and the spacer and hint stay pinned below throughout.
        let mut picker = picker(4, 3);
        picker.set_note("heads up");
        picker.set_viewport(3, picker.note_height(picker.width()));
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#111111|#555555|->  heads up                                \n",
            "<-|#555555|->                                          \n",
            "<#333333|#222222|->> item 0                                  \n",
            "<-|#555555|->                                          \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel\n",
        );
        picker.to_bottom();
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->  item 1                                  \n",
            "<#333333|#555555|->  item 2                                  \n",
            "<#333333|#222222|->> item 3                                  \n",
            "<-|#555555|->                                          \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel\n",
        );
    }

    #[test]
    fn a_long_label_is_clipped_to_the_content_width() {
        // A label wider than the given content width is cut to fit, and the hint
        // is clipped the same way rather than overflowing the box.
        let rows: Vec<Box<dyn PickerRow<Vec<usize>>>> = vec![Box::new(Row {
            label: "crates/wiff-tui/src/picker.rs".to_string(),
            extra: String::new(),
            index: 0,
        })];
        let mut picker = Picker::new("Files", rows, HINT, colors());
        picker.set_viewport(1, 0);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(13)),
            "<#333333|#222222|->> crates/wiff\n",
            "<-|#555555|->             \n",
            "<#444444|#555555|->  up/down mov\n",
        );
    }

    #[test]
    fn activating_runs_the_highlighted_rows_effect_on_the_host() {
        // The highlighted row, and only it, acts on the host when activated.
        let mut picker = picker(4, 4);
        picker.select_next();
        picker.select_next();
        let mut host = Vec::new();
        picker.activate_selected(&mut host);
        wince::assert_eq!(host, vec![2]);
    }

    #[test]
    fn a_filterable_picker_starts_in_navigation_mode_without_a_prompt() {
        // Until the reviewer presses `/` a filterable picker is a plain list: no
        // prompt, every row shown, and the hint names the `/` that filters.
        let picker = filter_picker(&[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")], 6);
        wince::assert_eq!(
            (picker.is_filterable(), picker.filtering(), picker.query()),
            (true, false, "")
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#222222|->> alpha.rs                                          \n",
            "<#333333|#555555|->  beta.rs                                           \n",
            "<#333333|#555555|->  gamma.rs                                          \n",
            "<-|#555555|->                                                    \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel  / filter\n",
        );
    }

    #[test]
    fn entering_filter_mode_reveals_the_prompt_and_the_filter_hint() {
        // Pressing `/` reveals the prompt line with just its cursor, keeps every
        // row, and swaps the navigation hint for the filter-mode hint.
        let mut picker = filter_picker(&[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")], 6);
        picker.begin_filter();
        wince::assert_eq!((picker.filtering(), picker.query()), (true, ""));
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->> █                                                 \n",
            "<#333333|#222222|->> alpha.rs                                          \n",
            "<#333333|#555555|->  beta.rs                                           \n",
            "<#333333|#555555|->  gamma.rs                                          \n",
            "<-|#555555|->                                                    \n",
            "<#444444|#555555|->  type to filter   esc navigate                     \n",
        );
    }

    #[test]
    fn typing_a_query_narrows_to_the_matches_and_paints_the_matched_characters() {
        // Typing "al" keeps only the row containing that subsequence, echoes the
        // query in the prompt, and paints its matched characters in the match
        // color while the rest of the label stays plain.
        let mut picker = filter_picker(&[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")], 6);
        type_query(&mut picker, "al");
        wince::assert_eq!(picker.query(), "al");
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->> al█                                               \n",
            "<#333333|#222222|->> <#666666|#222222|->al<#333333|#222222|->pha.rs<#333333|#222222|->                                          \n",
            "<-|#555555|->                                                    \n",
            "<#444444|#555555|->  type to filter   esc navigate                     \n",
        );
    }

    #[test]
    fn matching_a_non_ascii_label_keeps_the_row_without_painting_it() {
        // The label has a base letter plus a combining accent (two codepoints in
        // one grapheme), which throws off the matcher's character offsets. Rather
        // than risk painting the wrong glyphs the row is matched and kept but
        // drawn plain, with no characters picked out.
        let mut picker = filter_picker(&[("a\u{301}bc.rs", ""), ("other", "")], 6);
        type_query(&mut picker, "bc");
        wince::assert_eq!(picker.list_len(), 1);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->> bc█                                               \n",
            "<#333333|#222222|->> ábc.rs                                           \n",
            "<-|#555555|->                                                    \n",
            "<#444444|#555555|->  type to filter   esc navigate                     \n",
        );
    }

    #[test]
    fn backspacing_the_query_widens_the_list_back_to_every_row() {
        // Trimming the query one character at a time returns the list to all rows
        // once it empties, with the highlight back on the first.
        let mut picker = filter_picker(&[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")], 6);
        type_query(&mut picker, "be");
        wince::assert_eq!(picker.list_len(), 1);
        picker.pop_query();
        picker.pop_query();
        wince::assert_eq!(
            (picker.query(), picker.list_len(), picker.selected()),
            ("", 3, 0)
        );
    }

    #[test]
    fn escape_leaves_filter_mode_clearing_the_query_and_widening_the_list() {
        // After narrowing to one row, leaving filter mode drops the query, hides
        // the prompt, restores every row with the highlight on the first, and
        // brings back the navigation hint.
        let mut picker = filter_picker(&[("alpha.rs", ""), ("beta.rs", ""), ("gamma.rs", "")], 6);
        type_query(&mut picker, "be");
        wince::assert_eq!(picker.list_len(), 1);
        picker.end_filter();
        wince::assert_eq!(
            (
                picker.filtering(),
                picker.query(),
                picker.list_len(),
                picker.selected()
            ),
            (false, "", 3, 0)
        );
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#222222|->> alpha.rs                                          \n",
            "<#333333|#555555|->  beta.rs                                           \n",
            "<#333333|#555555|->  gamma.rs                                          \n",
            "<-|#555555|->                                                    \n",
            "<#444444|#555555|->  up/down move   enter select   esc cancel  / filter\n",
        );
    }

    #[test]
    fn a_query_matching_only_hidden_text_keeps_the_row_without_painting_it() {
        // "todo" appears only in a row's hidden match text, not its label, so the
        // row is kept but nothing in the visible label is highlighted.
        let mut picker = filter_picker(
            &[("alpha.rs", "the todo note"), ("beta.rs", "unrelated")],
            6,
        );
        type_query(&mut picker, "todo");
        wince::assert_eq!(picker.list_len(), 1);
        #[rustfmt::skip]
        wince::snapshot_str!(
            dump(&picker.lines(picker.width())),
            "<#333333|#555555|->> todo█                                             \n",
            "<#333333|#222222|->> alpha.rs                                          \n",
            "<-|#555555|->                                                    \n",
            "<#444444|#555555|->  type to filter   esc navigate                     \n",
        );
    }

    #[test]
    fn a_non_filterable_picker_ignores_filter_input() {
        // A plain picker cannot be filtered: it has no query, entering filter
        // mode is a no-op, and typing at it leaves every row listed.
        let mut picker = picker(3, 5);
        picker.begin_filter();
        picker.push_query('x');
        wince::assert_eq!(
            (
                picker.is_filterable(),
                picker.filtering(),
                picker.query(),
                picker.list_len()
            ),
            (false, false, "", 3)
        );
    }
}
