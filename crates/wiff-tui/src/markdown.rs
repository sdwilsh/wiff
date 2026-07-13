//! Rendering a saved comment body as markdown for display.
//!
//! A committed comment is shown formatted, not as raw markup: the reviewer
//! authors it in the inline editor, which highlights the markdown source as it
//! is typed, but wants the rendered result once it is saved. Colors come from
//! the active [`Theme`] and fenced code is highlighted through the same
//! [`Highlighter`] the diff uses, matching the rest of the interface.
//!
//! The renderer honors a GitHub-flavored subset: headings, ordered and
//! unordered lists, blockquotes, inline emphasis, links and images (shown as
//! `text (url)`), thematic breaks, tables, and fenced code. Column widths are
//! counted in characters, matching the rest of the renderer.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use wiff_diff::Highlighter;

use crate::render::color;
use crate::theme::Theme;
use crate::wrap::wrap_line;

/// Display width of a thematic-break rule.
const RULE_WIDTH: usize = 10;

/// The colors a rendered comment draws from, derived from the active theme.
#[derive(Clone, Copy)]
pub struct MarkdownColors {
    /// Body prose.
    pub text: Color,
    /// Heading text.
    pub heading: Color,
    /// Inline code spans.
    pub inline_code: Color,
    /// The fallback color for a fenced block whose language is unknown.
    pub code_block: Color,
    /// Table borders, blockquote gutters, and thematic-break rules.
    pub border: Color,
    /// Link and image text.
    pub link: Color,
    /// The parenthesized destination shown after a link or image.
    pub link_url: Color,
}

impl MarkdownColors {
    /// Derive the markdown palette from `theme`, reusing the comment, heading,
    /// and fold colors already chosen for the interface.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            text: color(theme.comment_fg),
            heading: color(theme.review_fg),
            inline_code: color(theme.hunk_header_fg),
            code_block: color(theme.comment_draft_fg),
            border: color(theme.fold_fg),
            link: color(theme.comment_author_fg),
            link_url: color(theme.fold_fg),
        }
    }
}

/// The display width of `s`, counted in characters to match the rest of the
/// renderer.
fn width(s: &str) -> usize {
    s.chars().count()
}

/// The markdown extensions we honor (GitHub-flavored subset).
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// Render `md` into styled lines wrapped to `width` columns, coloring with
/// `colors` and highlighting fenced code through `highlighter`. A `width` of
/// zero means the target width is not yet known, so nothing wraps, matching the
/// convention of [`wrap_line`].
pub fn render(
    md: &str,
    width: usize,
    colors: &MarkdownColors,
    highlighter: &Highlighter,
) -> Vec<Line<'static>> {
    let width = if width == 0 { usize::MAX } else { width };
    let mut renderer = Md::new(width, *colors, highlighter);
    for event in Parser::new_ext(md, options()) {
        renderer.event(event);
    }
    renderer.flush_inline();
    renderer.out
}

/// A per-line indent/gutter contributed by an enclosing list item or blockquote.
/// `head` prefixes the first emitted line (e.g. a bullet), `tail` the rest; both
/// share a display width so wrapping stays aligned.
struct Prefix {
    head: String,
    tail: String,
    style: Style,
    used: bool,
}

/// State for an open ordered/unordered list, tracking the next item number.
struct ListState {
    ordered: bool,
    next: u64,
}

/// A GFM table buffered whole so its columns can be measured and aligned before
/// anything is emitted. Each cell is the styled spans of its inline content;
/// `rows[0]` is the header. `row` accumulates the cells of the row in progress.
#[derive(Default)]
struct TableState {
    aligns: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
}

struct Md<'h> {
    width: usize,
    colors: MarkdownColors,
    highlighter: &'h Highlighter,
    out: Vec<Line<'static>>,
    /// Inline style stack (blockquote base, heading, emphasis, links, ...).
    styles: Vec<Style>,
    /// Line prefixes contributed by list items and blockquotes.
    prefixes: Vec<Prefix>,
    /// The current block's inline spans, wrapped and flushed at the block end.
    inline: Vec<Span<'static>>,
    /// Whether a blank separator line precedes the next block.
    need_blank: bool,
    lists: Vec<ListState>,
    /// The accumulated body of an open fenced/indented code block.
    code: Option<String>,
    /// The info string (language token) of the open fenced code block.
    code_lang: String,
    /// The destination URL and accumulated text of an open link.
    link: Option<(String, String)>,
    table: Option<TableState>,
}

impl<'h> Md<'h> {
    fn new(width: usize, colors: MarkdownColors, highlighter: &'h Highlighter) -> Self {
        Md {
            width: width.max(1),
            colors,
            highlighter,
            out: Vec::new(),
            styles: Vec::new(),
            prefixes: Vec::new(),
            inline: Vec::new(),
            need_blank: false,
            lists: Vec::new(),
            code: None,
            code_lang: String::new(),
            link: None,
            table: None,
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                let style = self
                    .cur()
                    .patch(Style::default().fg(self.colors.inline_code));
                self.inline.push(Span::styled(code.into_string(), style));
            }
            Event::SoftBreak => self.inline.push(Span::raw(" ")),
            Event::HardBreak => self.flush_inline(),
            Event::Rule => {
                self.begin_block();
                // Kept short: full-width rules reflow badly on terminal resize.
                let rule = "\u{2500}".repeat(self.avail().min(RULE_WIDTH));
                self.emit_line(vec![Span::styled(
                    rule,
                    Style::default().fg(self.colors.border),
                )]);
                self.need_blank = true;
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                self.inline
                    .push(Span::styled(marker.to_string(), self.cur()));
            }
            // Inline raw HTML: keep the source text rather than dropping it.
            Event::InlineHtml(html) => self.text(&html),
            // Block raw HTML: emit its lines verbatim (framed by HtmlBlock).
            Event::Html(html) => {
                for line in html.lines() {
                    self.emit_line(vec![Span::raw(line.to_string())]);
                }
            }
            // Math and footnote extensions are disabled, so those never reach
            // here; any remaining event has no renderable text.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.begin_block(),
            Tag::Heading { level, .. } => {
                self.begin_block();
                self.styles.push(self.heading_style(level));
                if level_num(level) >= 3 {
                    let marker = format!("{} ", "#".repeat(level_num(level)));
                    self.inline.push(Span::styled(marker, self.cur()));
                }
            }
            Tag::BlockQuote(_) => {
                self.begin_block();
                self.prefixes.push(Prefix {
                    head: "\u{258f} ".to_string(),
                    tail: "\u{258f} ".to_string(),
                    style: Style::default().fg(self.colors.border),
                    used: false,
                });
                self.styles.push(
                    Style::default()
                        .fg(self.colors.border)
                        .add_modifier(Modifier::ITALIC),
                );
            }
            Tag::CodeBlock(kind) => {
                self.begin_block();
                self.code = Some(String::new());
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Tag::List(start) => {
                self.begin_block();
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                let marker = match self.lists.last_mut() {
                    Some(list) if list.ordered => {
                        let marker = format!("{}. ", list.next);
                        list.next += 1;
                        marker
                    }
                    _ => "- ".to_string(),
                };
                let tail = " ".repeat(width(&marker));
                self.prefixes.push(Prefix {
                    head: marker,
                    tail,
                    // Match the body text; the terminal default would otherwise
                    // make the bullet stand out more strongly than its item.
                    style: Style::default().fg(self.colors.text),
                    used: false,
                });
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.styles.push(
                    Style::default()
                        .fg(self.colors.link)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.link = Some((dest_url.into_string(), String::new()));
            }
            Tag::Table(aligns) => {
                self.begin_block();
                self.table = Some(TableState {
                    aligns,
                    ..TableState::default()
                });
            }
            // Header cells render bold; push the modifier so the head's inline
            // content (accumulated in `self.inline` like any other block) picks
            // it up via `cur()`.
            Tag::TableHead => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            // A cell's inline content flows through the normal `self.inline`
            // path and is claimed whole at `TagEnd::TableCell`.
            Tag::TableRow | Tag::TableCell => {}
            Tag::HtmlBlock => self.begin_block(),
            Tag::Image { dest_url, .. } => {
                // The alt text streams in as `Text`; capture the URL so the
                // image is shown as `alt (url)` rather than silently dropped.
                self.link = Some((dest_url.into_string(), String::new()));
            }
            // Footnote definitions, metadata blocks, etc. have no inline text.
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
                self.need_blank = true;
            }
            TagEnd::Heading(_) => {
                self.flush_inline();
                self.styles.pop();
                self.need_blank = true;
            }
            TagEnd::BlockQuote(_) => {
                self.styles.pop();
                self.prefixes.pop();
                self.need_blank = true;
            }
            TagEnd::CodeBlock => {
                if let Some(buf) = self.code.take() {
                    let body_width = self.avail().max(1);
                    let body = buf.strip_suffix('\n').unwrap_or(&buf);
                    let lang = std::mem::take(&mut self.code_lang);
                    self.emit_code(&lang, body, body_width);
                }
                self.need_blank = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.need_blank = true;
            }
            TagEnd::Item => {
                // Tight list items hold their text directly (no paragraph), so
                // flush any pending inline content before closing the item.
                self.flush_inline();
                // An empty item still shows its marker: a bare reply like "8."
                // parses as an ordered list with one empty item, and dropping it
                // would render the whole message as nothing.
                if self.prefixes.last().is_some_and(|prefix| !prefix.used) {
                    self.emit_line(Vec::new());
                }
                self.prefixes.pop();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                self.append_link_url();
            }
            // Images push no style; just append the captured URL after the alt.
            TagEnd::Image => self.append_link_url(),
            TagEnd::HtmlBlock => self.need_blank = true,
            TagEnd::TableCell => {
                let spans = std::mem::take(&mut self.inline);
                if let Some(table) = &mut self.table {
                    table.row.push(spans);
                }
            }
            // The header's cells sit directly under `TableHead` (no `TableRow`),
            // so both close the row the same way: move the accumulated cells to
            // `rows`. `rows[0]` is thus the header.
            TagEnd::TableRow | TagEnd::TableHead => {
                if matches!(tag, TagEnd::TableHead) {
                    self.styles.pop();
                }
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
                self.need_blank = true;
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(buf) = &mut self.code {
            buf.push_str(text);
            return;
        }
        if let Some((_, link_text)) = &mut self.link {
            link_text.push_str(text);
        }
        let style = self.cur();
        self.inline.push(Span::styled(text.to_string(), style));
    }

    /// Append the open link/image destination as ` (url)` unless the visible
    /// text already is the URL. Clears the pending link/image.
    fn append_link_url(&mut self) {
        if let Some((url, text)) = self.link.take()
            && !url.is_empty()
            && text != url
        {
            self.inline.push(Span::styled(
                format!(" ({url})"),
                Style::default().fg(self.colors.link_url),
            ));
        }
    }

    /// The effective inline style: the body color patched by the style stack.
    fn cur(&self) -> Style {
        let mut style = Style::default().fg(self.colors.text);
        for pushed in &self.styles {
            style = style.patch(*pushed);
        }
        style
    }

    /// Columns available for content after the active line prefixes.
    fn avail(&self) -> usize {
        let prefix: usize = self.prefixes.iter().map(|p| width(&p.tail)).sum();
        self.width.saturating_sub(prefix).max(1)
    }

    /// The bold, colored style for a heading of `level`; the top level is also
    /// underlined.
    fn heading_style(&self, level: HeadingLevel) -> Style {
        let style = Style::default()
            .fg(self.colors.heading)
            .add_modifier(Modifier::BOLD);
        if level_num(level) == 1 {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }

    /// Start a new block: flush any pending inline (e.g. a tight list item's
    /// text that precedes a nested block), then emit a blank separator when one
    /// is pending.
    fn begin_block(&mut self) {
        self.flush_inline();
        if self.need_blank && !self.out.is_empty() {
            self.out.push(Line::from(String::new()));
        }
        self.need_blank = false;
    }

    /// The prefix spans for the next emitted line, consuming each prefix's head
    /// form once so only the first line of an item shows its bullet.
    fn line_prefix(&mut self) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for prefix in &mut self.prefixes {
            let text = if prefix.used {
                prefix.tail.clone()
            } else {
                prefix.used = true;
                prefix.head.clone()
            };
            if !text.is_empty() {
                spans.push(Span::styled(text, prefix.style));
            }
        }
        spans
    }

    /// Emit one line of `content`, prefixed with the active list/quote gutters.
    fn emit_line(&mut self, mut content: Vec<Span<'static>>) {
        let mut spans = self.line_prefix();
        spans.append(&mut content);
        self.out.push(Line::from(spans));
    }

    /// Emit a fenced code block body. When `lang` names a known syntax the body
    /// is highlighted; otherwise each source line is shown in a flat code color.
    /// Either way a line wider than `body_width` wraps rather than overflowing.
    fn emit_code(&mut self, lang: &str, body: &str, body_width: usize) {
        if let Some(lines) = self.highlighter.highlight_code(lang, body) {
            for spans in lines {
                let line = Line::from(
                    spans
                        .iter()
                        .map(|span| Span::styled(span.text.clone(), style_of(&span.style)))
                        .collect::<Vec<_>>(),
                );
                for wrapped in wrap_line(&line, body_width) {
                    self.emit_line(wrapped.spans);
                }
            }
            return;
        }
        let flat = Style::default().fg(self.colors.code_block);
        for source_line in body.split('\n') {
            let line = Line::from(Span::styled(source_line.to_string(), flat));
            for wrapped in wrap_line(&line, body_width) {
                self.emit_line(wrapped.spans);
            }
        }
    }

    /// Lay out a buffered table into aligned, box-drawn lines and emit them.
    fn emit_table(&mut self, table: TableState) {
        let TableState { aligns, rows, .. } = table;
        if rows.is_empty() {
            return;
        }
        // Columns are defined by the widest row (the header, normally); ragged
        // input just means some rows have blank trailing cells.
        let ncols = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(aligns.len());
        if ncols == 0 {
            return;
        }
        let border = self.colors.border;
        for line in layout_table(&rows, &aligns, ncols, self.avail(), border) {
            self.emit_line(line);
        }
    }

    /// Wrap and emit the pending inline spans as the current block's lines.
    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let width = self.avail();
        let spans = std::mem::take(&mut self.inline);
        for line in wrap_spans(&spans, width) {
            self.emit_line(line);
        }
    }
}

/// A word paired with its style and whether whitespace precedes it, so wrapping can
/// re-insert separators only where the source had them (adjacent spans like a
/// strikethrough word followed by punctuation must not gain a space).
struct Word {
    text: String,
    style: Style,
    leading_space: bool,
}

/// Greedy word-wrap of styled spans to `width`, breaking at spaces. Runs of
/// same-styled words merge into one span; words longer than `width` overflow.
fn wrap_spans(spans: &[Span<'static>], width_cols: usize) -> Vec<Vec<Span<'static>>> {
    let width_cols = width_cols.max(1);
    let words = tokenize(spans);
    let mut lines: Vec<Vec<Word>> = Vec::new();
    let mut cur: Vec<Word> = Vec::new();
    let mut cur_width = 0usize;
    for mut word in words {
        let word_width = width(&word.text);
        let sep = usize::from(word.leading_space && !cur.is_empty());
        if !cur.is_empty() && cur_width + sep + word_width > width_cols {
            lines.push(std::mem::take(&mut cur));
            cur_width = 0;
        }
        if cur.is_empty() {
            word.leading_space = false;
            cur_width = word_width;
        } else {
            cur_width += sep + word_width;
        }
        cur.push(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines.into_iter().map(merge_words).collect()
}

/// Split styled spans into words, recording for each whether a space separated
/// it from the previous token so wrapping and merging can reproduce the source
/// spacing exactly.
fn tokenize(spans: &[Span<'static>]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut pending_space = false;
    for span in spans {
        for (i, piece) in span.content.split(' ').enumerate() {
            if i > 0 {
                pending_space = true;
            }
            if !piece.is_empty() {
                words.push(Word {
                    text: piece.to_string(),
                    style: span.style,
                    leading_space: pending_space,
                });
                pending_space = false;
            }
        }
    }
    words
}

/// Merge a line's words into spans, joining same-styled neighbors and inserting
/// a plain space only where a separator is needed between differing styles.
fn merge_words(words: Vec<Word>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur: Option<(Style, String)> = None;
    for word in words {
        match cur.take() {
            None => cur = Some((word.style, word.text)),
            Some((style, mut text)) if style == word.style => {
                if word.leading_space {
                    text.push(' ');
                }
                text.push_str(&word.text);
                cur = Some((style, text));
            }
            Some((style, text)) => {
                spans.push(Span::styled(text, style));
                if word.leading_space {
                    spans.push(Span::raw(" "));
                }
                cur = Some((word.style, word.text));
            }
        }
    }
    if let Some((style, text)) = cur {
        spans.push(Span::styled(text, style));
    }
    spans
}

/// The min (widest single word) and max (whole cell on one line) content widths
/// of a table cell, used to drive auto column layout.
fn cell_extent(spans: &[Span<'static>]) -> (usize, usize) {
    let mut min = 0usize;
    let mut max = 0usize;
    for word in tokenize(spans) {
        let w = width(&word.text);
        min = min.max(w);
        if max > 0 && word.leading_space {
            max += 1;
        }
        max += w;
    }
    (min, max)
}

/// Choose column widths for a total content `budget` (the terminal width less
/// the border/padding chrome) using classic auto table layout: give every
/// column its one-line width when they all fit; when over-constrained give each
/// its minimum and let the table overflow rather than break words mid-token;
/// otherwise hand the slack above the minimums out in proportion to each
/// column's growth room (`max - min`).
fn distribute(mins: &[usize], maxs: &[usize], budget: usize) -> Vec<usize> {
    let total_min: usize = mins.iter().sum();
    let total_max: usize = maxs.iter().sum();
    if total_max <= budget {
        return maxs.to_vec();
    }
    if total_min >= budget {
        return mins.to_vec();
    }
    let slack = budget - total_min;
    // `growth > slack > 0` here (since `total_max > budget`), so the division is
    // safe and every column's share stays strictly below its room, leaving each
    // column able to absorb one unit of the rounding remainder below.
    let growth: usize = maxs.iter().zip(mins).map(|(mx, mn)| mx - mn).sum();
    let mut widths = mins.to_vec();
    let mut remainder = slack;
    for (c, w) in widths.iter_mut().enumerate() {
        let share = slack * (maxs[c] - mins[c]) / growth;
        *w += share;
        remainder -= share;
    }
    for w in widths.iter_mut() {
        if remainder == 0 {
            break;
        }
        *w += 1;
        remainder -= 1;
    }
    widths
}

/// Lay out `rows` (with `rows[0]` the header) into box-drawn, column-aligned
/// lines wrapped to fit `width` columns, drawing the box chrome in `border`.
fn layout_table(
    rows: &[Vec<Vec<Span<'static>>>],
    aligns: &[Alignment],
    ncols: usize,
    width_cols: usize,
    border: Color,
) -> Vec<Vec<Span<'static>>> {
    // Per-column min/max content widths across every row's cell in that column.
    let mut mins = vec![1usize; ncols];
    let mut maxs = vec![1usize; ncols];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let (min, max) = cell_extent(cell);
            mins[c] = mins[c].max(min);
            maxs[c] = maxs[c].max(max);
        }
    }
    // Chrome is a border on each side of the table plus one between every pair
    // of columns (`ncols + 1` bars), and a space of padding either side of each
    // column (`2 * ncols`).
    let chrome = 3 * ncols + 1;
    let widths = distribute(&mins, &maxs, width_cols.saturating_sub(chrome));

    let mut out = Vec::new();
    out.push(border_line(
        &widths, '\u{250c}', '\u{252c}', '\u{2510}', border,
    ));
    out.extend(render_row(&rows[0], &widths, aligns, ncols, border));
    // A rule follows the header and separates every pair of body rows.
    for row in &rows[1..] {
        out.push(border_line(
            &widths, '\u{251c}', '\u{253c}', '\u{2524}', border,
        ));
        out.extend(render_row(row, &widths, aligns, ncols, border));
    }
    out.push(border_line(
        &widths, '\u{2514}', '\u{2534}', '\u{2518}', border,
    ));
    out
}

/// A horizontal box-drawing rule spanning `widths` with the given left, inner
/// junction, and right corner characters, drawn in `border`.
fn border_line(
    widths: &[usize],
    left: char,
    mid: char,
    right: char,
    border: Color,
) -> Vec<Span<'static>> {
    let mut s = String::new();
    s.push(left);
    for (c, &w) in widths.iter().enumerate() {
        // `w + 2` covers the column plus its padding space on each side.
        s.extend(std::iter::repeat_n('\u{2500}', w + 2));
        s.push(if c + 1 == widths.len() { right } else { mid });
    }
    vec![Span::styled(s, Style::default().fg(border))]
}

/// Render one table row into its (possibly multiple, when cells wrap) visual
/// lines, each framed and separated by vertical box-drawing bars in `border`.
fn render_row(
    row: &[Vec<Span<'static>>],
    widths: &[usize],
    aligns: &[Alignment],
    ncols: usize,
    border: Color,
) -> Vec<Vec<Span<'static>>> {
    // Wrap each column's cell to its width; an empty cell still occupies a line.
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = (0..ncols)
        .map(|c| {
            let cell = row.get(c).map(Vec::as_slice).unwrap_or(&[]);
            let lines = wrap_spans(cell, widths[c]);
            if lines.is_empty() {
                vec![Vec::new()]
            } else {
                lines
            }
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let bar = || Span::styled("\u{2502}".to_string(), Style::default().fg(border));
    let mut out = Vec::new();
    for line_ix in 0..height {
        let mut spans = vec![bar()];
        for c in 0..ncols {
            let content = wrapped[c].get(line_ix).map(Vec::as_slice).unwrap_or(&[]);
            let align = aligns.get(c).copied().unwrap_or(Alignment::None);
            spans.push(Span::raw(" "));
            pad_cell(&mut spans, content, widths[c], align);
            spans.push(Span::raw(" "));
            spans.push(bar());
        }
        out.push(spans);
    }
    out
}

/// Append `content` padded to `width` columns onto `out`, distributing the
/// padding per the column `align` (`None` renders as left).
fn pad_cell(
    out: &mut Vec<Span<'static>>,
    content: &[Span<'static>],
    width_cols: usize,
    align: Alignment,
) {
    let used: usize = content.iter().map(|span| width(&span.content)).sum();
    let pad = width_cols.saturating_sub(used);
    let (left, right) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::Left | Alignment::None => (0, pad),
    };
    if left > 0 {
        out.push(Span::raw(" ".repeat(left)));
    }
    out.extend(content.iter().cloned());
    if right > 0 {
        out.push(Span::raw(" ".repeat(right)));
    }
}

fn level_num(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Translate a highlighter [`Style`](wiff_diff::Style) into a ratatui style.
fn style_of(style: &wiff_diff::Style) -> Style {
    let mut out = Style::default().fg(color(style.fg));
    if style.bold {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.italic {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;
    use wiff_diff::Highlighter;

    use super::*;
    use crate::render::testutil::TEST_THEME;

    /// A palette of distinct, recognizable colors so the annotated output names
    /// each role. Body text is `Reset` so plain prose renders untagged.
    fn palette() -> MarkdownColors {
        MarkdownColors {
            text: Color::Reset,
            heading: Color::Yellow,
            inline_code: Color::Cyan,
            code_block: Color::Green,
            border: Color::Gray,
            link: Color::Blue,
            link_url: Color::DarkGray,
        }
    }

    /// Render markdown to an annotated, human-readable multi-line string: each
    /// span is prefixed with its non-default style as `<flags>` so assertions
    /// capture both text and styling.
    fn show(md: &str, width: usize) -> String {
        let highlighter = Highlighter::with_theme(TEST_THEME).expect("theme");
        render(md, width, &palette(), &highlighter)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| format!("{}{}", tag(span.style), span.content))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tag(style: Style) -> String {
        let mut flags = Vec::new();
        if let Some(color) = style.fg {
            let name = color_name(color);
            if !name.is_empty() {
                flags.push(name);
            }
        }
        if style.add_modifier.contains(Modifier::BOLD) {
            flags.push("bold".to_string());
        }
        if style.add_modifier.contains(Modifier::ITALIC) {
            flags.push("italic".to_string());
        }
        if style.add_modifier.contains(Modifier::UNDERLINED) {
            flags.push("underline".to_string());
        }
        if style.add_modifier.contains(Modifier::CROSSED_OUT) {
            flags.push("strike".to_string());
        }
        if flags.is_empty() {
            String::new()
        } else {
            format!("<{}>", flags.join(","))
        }
    }

    fn color_name(color: Color) -> String {
        match color {
            Color::Reset => "",
            Color::Yellow => "yellow",
            Color::Cyan => "cyan",
            Color::Green => "green",
            Color::Gray => "gray",
            Color::Blue => "blue",
            Color::DarkGray => "dimgray",
            other => return format!("{other:?}"),
        }
        .to_string()
    }

    #[test]
    fn paragraph_with_inline_styles() {
        let out = show("A **bold** and *italic* and `code` and ~~gone~~.", 80);
        wince::snapshot_str!(
            out,
            "A <bold>bold and <italic>italic and <cyan>code and <strike>gone."
        );
    }

    #[test]
    fn headings_by_level() {
        let out = show("# One\n\n## Two\n\n### Three", 80);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "<yellow,bold,underline>One\n",
            "\n",
            "<yellow,bold>Two\n",
            "\n",
            "<yellow,bold>### Three",
        );
    }

    #[test]
    fn unordered_and_ordered_lists() {
        let out = show("- first\n- second\n\n1. one\n2. two", 80);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "- first\n",
            "- second\n",
            "\n",
            "1. one\n",
            "2. two",
        );
    }

    #[test]
    fn bare_ordered_marker_still_renders() {
        // A whole reply of "8." parses as an ordered list with a single empty
        // item; it must show its marker rather than render as nothing.
        wince::snapshot_str!(show("8.", 80), "8. ");
    }

    #[test]
    fn empty_item_between_filled_ones_keeps_its_marker() {
        #[rustfmt::skip]
        wince::snapshot_str!(
            show("1. one\n2.\n3. three", 80),
            "1. one\n",
            "2. \n",
            "3. three",
        );
    }

    #[test]
    fn nested_list_indents() {
        let out = show("- outer\n    - inner", 80);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "- outer\n",
            "  - inner",
        );
    }

    #[test]
    fn task_list_markers() {
        let out = show("- [x] done\n- [ ] todo", 80);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "- [x] done\n",
            "- [ ] todo",
        );
    }

    #[test]
    fn blockquote_has_gutter_and_italic() {
        let out = show("> quoted text", 80);
        wince::snapshot_str!(out, "<gray>▏ <gray,italic>quoted text");
    }

    #[test]
    fn fenced_code_block_is_highlighted_without_a_border() {
        // The fence delimiter lines are dropped; the body is highlighted by its
        // info-string language.
        let out = show("```rust\nfn main() {}\n```", 80);
        wince::snapshot_str!(
            out,
            "<Rgb(180, 142, 173)>fn<Rgb(192, 197, 206)> <Rgb(143, 161, 179)>main<Rgb(192, 197, 206)>(<Rgb(192, 197, 206)>)<Rgb(192, 197, 206)> <Rgb(192, 197, 206)>{<Rgb(192, 197, 206)>}"
        );
    }

    #[test]
    fn fenced_code_block_unknown_language_is_flat_without_a_border() {
        // An info string that names no known syntax falls back to the flat code
        // color, still with no fence delimiter lines.
        let out = show("```nonesuch\nfn main() {}\n```", 80);
        wince::snapshot_str!(out, "<green>fn main() {}");
    }

    #[test]
    fn link_appends_url_when_text_differs() {
        let out = show("see [docs](https://example.com)", 80);
        wince::snapshot_str!(
            out,
            "see <blue,underline>docs <dimgray>(https://example.com)"
        );
    }

    #[test]
    fn autolink_omits_redundant_url() {
        let out = show("<https://example.com>", 80);
        wince::snapshot_str!(out, "<blue,underline>https://example.com");
    }

    #[test]
    fn image_shows_alt_text_and_url() {
        let out = show("![alt text](img.png)", 80);
        wince::snapshot_str!(out, "alt text <dimgray>(img.png)");
    }

    #[test]
    fn thematic_break_is_a_rule() {
        let out = show("a\n\n---\n\nb", 10);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "a\n",
            "\n",
            "<gray>──────────\n",
            "\n",
            "b",
        );
    }

    #[test]
    fn wraps_to_width() {
        let out = show("one two three four five", 12);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "one two\n",
            "three four\n",
            "five",
        );
    }

    #[test]
    fn a_zero_width_leaves_prose_unwrapped() {
        // Width zero means the target width is unknown, so nothing wraps.
        let out = show("one two three four five", 0);
        wince::snapshot_str!(out, "one two three four five");
    }

    #[test]
    fn table_columns_align_with_box_borders() {
        let out = show("| a | b |\n| - | - |\n| 1 | 2 |", 80);
        #[rustfmt::skip]
        wince::snapshot_str!(
            out,
            "<gray>┌───┬───┐\n",
            "<gray>│ <bold>a <gray>│ <bold>b <gray>│\n",
            "<gray>├───┼───┤\n",
            "<gray>│ 1 <gray>│ 2 <gray>│\n",
            "<gray>└───┴───┘",
        );
    }

    #[test]
    fn inline_html_is_shown_raw() {
        let out = show("before <br> after", 80);
        wince::snapshot_str!(out, "before <br> after");
    }

    #[test]
    fn empty_input_renders_nothing() {
        let highlighter = Highlighter::with_theme(TEST_THEME).expect("theme");
        wince::assert_eq!(
            render("", 80, &palette(), &highlighter),
            Vec::<Line<'static>>::new()
        );
    }
}
