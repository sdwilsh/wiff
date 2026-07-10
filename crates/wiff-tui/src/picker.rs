//! A reusable modal list the reviewer scrolls and selects from.
//!
//! A [`Picker`] floats over the review, listing labeled rows one per line with
//! one highlighted. The highlight moves with the same navigation the main view
//! uses, and enter activates the highlighted row. What a row does on activation
//! is its own concern: each row is a [`PickerRow`] over the host it mutates, so
//! the widget itself knows nothing of files, comments, or themes and the same
//! modal drives all of them. A list taller than the space it is given scrolls,
//! keeping the highlighted row in view.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wiff_diff::Rgb;

use crate::render::color;

/// One row of a picker: the text it shows and what selecting it does to the
/// host `Ctx`. The widget holds these as trait objects so a single picker can
/// list rows of any kind, each carrying its own activation.
pub trait PickerRow<Ctx> {
    /// The text shown for this row.
    fn label(&self) -> String;

    /// Act on the host now that this row has been chosen.
    fn activate(self: Box<Self>, ctx: &mut Ctx);
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
    /// The color of the key hint along the bottom.
    pub hint: Rgb,
}

/// A modal list of [`PickerRow`]s over a host `Ctx`, with one row highlighted
/// and a scroll window that keeps it in view.
pub struct Picker<Ctx> {
    title: String,
    rows: Vec<Box<dyn PickerRow<Ctx>>>,
    selected: usize,
    /// The first row of the visible window into `rows`.
    top: usize,
    /// The number of rows the window shows, set by the host each frame from the
    /// space the modal is given.
    height: usize,
    /// The key hint shown along the bottom, naming the reviewer's own bound
    /// keys, built by the host.
    hint: String,
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
        Self {
            title: title.to_string(),
            rows,
            selected: 0,
            top: 0,
            height: 0,
            hint: hint.to_string(),
            colors,
        }
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

    /// Set how many rows the window shows, keeping the highlight in view.
    pub fn set_height(&mut self, height: usize) {
        self.height = height;
        self.scroll_into_view();
    }

    /// Consume the picker, acting on the host with the highlighted row. Does
    /// nothing when the picker is empty.
    pub fn activate_selected(self, ctx: &mut Ctx) {
        if let Some(row) = self.rows.into_iter().nth(self.selected) {
            row.activate(ctx);
        }
    }

    /// The number of rows in the list.
    pub fn list_len(&self) -> usize {
        self.rows.len()
    }

    /// The index of the highlighted row.
    pub fn selected(&self) -> usize {
        self.selected
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
    /// highlight marker, then the widest label or the hint.
    pub fn width(&self) -> usize {
        let widest = self
            .rows
            .iter()
            .map(|row| row.label().chars().count())
            .max()
            .unwrap_or(0);
        2 + widest.max(self.hint.chars().count())
    }

    /// The modal's content lines for a content width of `width`: the visible
    /// window of rows, each marked and highlighted when it is the selection,
    /// then a spacer and the key hint. Labels and the hint are clipped to fit.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let label_width = width.saturating_sub(2);
        let background = color(self.colors.background);
        let end = (self.top + self.height).min(self.rows.len());
        let mut lines: Vec<Line<'static>> = (self.top..end)
            .map(|index| {
                let selected = index == self.selected;
                let marker = if selected { "> " } else { "  " };
                let label = clip(&self.rows[index].label(), label_width);
                let text = format!("{marker}{label:<label_width$}");
                let row_bg = if selected {
                    color(self.colors.selected_bg)
                } else {
                    background
                };
                Line::from(Span::styled(
                    text,
                    Style::default().fg(color(self.colors.text)).bg(row_bg),
                ))
            })
            .collect();
        lines.push(Line::from(Span::styled(
            " ".repeat(width),
            Style::default().bg(background),
        )));
        lines.push(Line::from(Span::styled(
            format!("  {:<label_width$}", clip(&self.hint, label_width)),
            Style::default().fg(color(self.colors.hint)).bg(background),
        )));
        lines
    }

    /// The last addressable row, or zero for an empty list.
    fn last_row(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    /// A window's worth of rows for a page move, at least one.
    fn page(&self) -> usize {
        self.height.max(1)
    }

    /// Slide the window so the highlighted row stays visible.
    fn scroll_into_view(&mut self) {
        if self.height == 0 {
            return;
        }
        if self.selected < self.top {
            self.top = self.selected;
        }
        let bottom = self.selected + 1;
        if bottom > self.top + self.height {
            self.top = bottom - self.height;
        }
        let max_top = self.rows.len().saturating_sub(self.height);
        self.top = self.top.min(max_top);
    }
}

/// Clip `text` to at most `width` columns.
fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::{Picker, PickerColors, PickerRow};
    use crate::render::testutil::dump;
    use wiff_diff::Rgb;

    /// A row naming itself, recording its index into the host when activated.
    struct Row {
        label: String,
        index: usize,
    }

    impl PickerRow<Vec<usize>> for Row {
        fn label(&self) -> String {
            self.label.clone()
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
                    index,
                }) as Box<dyn PickerRow<Vec<usize>>>
            })
            .collect();
        let mut picker = Picker::new("Files", rows, HINT, colors());
        picker.set_height(height);
        picker
    }

    /// A fixed hint for the tests, so the rendered width does not depend on the
    /// keymap the host would build it from.
    const HINT: &str = "up/down move   enter select   esc cancel";

    #[test]
    fn the_first_row_opens_highlighted_and_the_hint_sits_below() {
        // A short list fits its window whole: the first row opens marked and
        // washed, the rest plain, then a spacer and the key hint.
        let picker = picker(3, 5);
        let expected = "\
<#333333|#222222|->> item 0                                  
<#333333|#555555|->  item 1                                  
<#333333|#555555|->  item 2                                  
<-|#555555|->                                          
<#444444|#555555|->  up/down move   enter select   esc cancel
";
        k9::assert_equal!(dump(&picker.lines(picker.width())), expected.to_string());
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
        k9::assert_equal!(picker.selected(), 3);
        let expected = "\
<#333333|#555555|->  item 1                                  
<#333333|#555555|->  item 2                                  
<#333333|#222222|->> item 3                                  
<-|#555555|->                                          
<#444444|#555555|->  up/down move   enter select   esc cancel
";
        k9::assert_equal!(dump(&picker.lines(picker.width())), expected.to_string());
    }

    #[test]
    fn selecting_the_next_row_stops_at_the_last_and_bottom_jumps_there() {
        // Stepping down never runs off the end, and to_bottom lands on the last
        // row with the window scrolled to show it.
        let mut picker = picker(4, 2);
        for _ in 0..10 {
            picker.select_next();
        }
        k9::assert_equal!(picker.selected(), 3);
        picker.to_top();
        k9::assert_equal!(picker.selected(), 0);
        picker.to_bottom();
        k9::assert_equal!(picker.selected(), 3);
        let expected = "\
<#333333|#555555|->  item 2                                  
<#333333|#222222|->> item 3                                  
<-|#555555|->                                          
<#444444|#555555|->  up/down move   enter select   esc cancel
";
        k9::assert_equal!(dump(&picker.lines(picker.width())), expected.to_string());
    }

    #[test]
    fn a_long_label_is_clipped_to_the_content_width() {
        // A label wider than the given content width is cut to fit, and the hint
        // is clipped the same way rather than overflowing the box.
        let rows: Vec<Box<dyn PickerRow<Vec<usize>>>> = vec![Box::new(Row {
            label: "crates/wiff-tui/src/picker.rs".to_string(),
            index: 0,
        })];
        let mut picker = Picker::new("Files", rows, HINT, colors());
        picker.set_height(1);
        let expected = "\
<#333333|#222222|->> crates/wiff
<-|#555555|->             
<#444444|#555555|->  up/down mov
";
        k9::assert_equal!(dump(&picker.lines(13)), expected.to_string());
    }

    #[test]
    fn activating_runs_the_highlighted_rows_effect_on_the_host() {
        // The highlighted row, and only it, acts on the host when activated.
        let mut picker = picker(4, 4);
        picker.select_next();
        picker.select_next();
        let mut host = Vec::new();
        picker.activate_selected(&mut host);
        k9::assert_equal!(host, vec![2]);
    }
}
