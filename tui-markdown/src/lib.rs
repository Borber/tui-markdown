//! A simple markdown renderer widget for Ratatui.
//!
//! This module provides a simple markdown renderer widget for Ratatui. It uses the `pulldown-cmark`
//! crate to parse markdown and convert it to a `Text` widget. The `Text` widget can then be
//! rendered to the terminal using the 'Ratatui' library.
#![cfg_attr(feature = "document-features", doc = "\n# Features")]
#![cfg_attr(feature = "document-features", doc = document_features::document_features!())]
//! # Example
//!
//! ~~~
//! use ratatui::text::Text;
//! use tui_markdown::from_str;
//!
//! # fn draw(frame: &mut ratatui::Frame) {
//! let markdown = r#"
//! This is a simple markdown renderer for Ratatui.
//!
//! - List item 1
//! - List item 2
//!
//! ```rust
//! fn main() {
//!     println!("Hello, world!");
//! }
//! ```
//! "#;
//!
//! let text = from_str(markdown);
//! frame.render_widget(text, frame.area());
//! # }
//! ~~~

#[cfg(feature = "highlight-code")]
use std::sync::LazyLock;
use std::vec;

#[cfg(feature = "highlight-code")]
use ansi_to_tui::IntoText;
use itertools::{Itertools, Position};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, CowStr, Event, HeadingLevel, Options as ParseOptions,
    Parser, Tag, TagEnd,
};
use ratatui_core::style::{Style, Stylize};
use ratatui_core::text::{Line, Span, Text};
#[cfg(feature = "highlight-code")]
use syntect::{
    easy::HighlightLines,
    highlighting::ThemeSet,
    parsing::SyntaxSet,
    util::{as_24_bit_terminal_escaped, LinesWithEndings},
};
use tracing::{debug, instrument, warn};
use unicode_width::UnicodeWidthChar;

pub use crate::options::Options;
pub use crate::style_sheet::{DefaultStyleSheet, StyleSheet};

mod options;
mod style_sheet;

/// Render Markdown `input` into a [`ratatui::text::Text`] using the default [`Options`].
///
/// This is a convenience function that uses the default options, which are defined in
/// [`Options::default`]. It is suitable for most use cases where you want to render Markdown
pub fn from_str(input: &str) -> Text<'_> {
    from_str_with_options(input, &Options::default())
}

/// Render Markdown `input` into a [`ratatui::text::Text`] using the supplied [`Options`].
///
/// This allows you to customize the styles and other rendering options.
///
/// # Example
///
/// ```
/// use tui_markdown::{from_str_with_options, DefaultStyleSheet, Options};
///
/// let input = "This is a **bold** text.";
/// let options = Options::default();
/// let text = from_str_with_options(input, &options);
/// ```
pub fn from_str_with_options<'a, S>(input: &'a str, options: &Options<S>) -> Text<'a>
where
    S: StyleSheet,
{
    let mut parse_opts = ParseOptions::empty();
    parse_opts.insert(ParseOptions::ENABLE_STRIKETHROUGH);
    parse_opts.insert(ParseOptions::ENABLE_TABLES);
    parse_opts.insert(ParseOptions::ENABLE_TASKLISTS);
    parse_opts.insert(ParseOptions::ENABLE_HEADING_ATTRIBUTES);
    parse_opts.insert(ParseOptions::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    parse_opts.insert(ParseOptions::ENABLE_SUPERSCRIPT);
    parse_opts.insert(ParseOptions::ENABLE_SUBSCRIPT);
    let parser = Parser::new_ext(input, parse_opts);

    let mut writer = TextWriter::new(parser, options.styles.clone());
    writer.run();
    writer.text
}

// Heading attributes collected from pulldown-cmark to render after the heading text.
struct HeadingMeta<'a> {
    id: Option<CowStr<'a>>,
    classes: Vec<CowStr<'a>>,
    attrs: Vec<(CowStr<'a>, Option<CowStr<'a>>)>,
}

impl<'a> HeadingMeta<'a> {
    fn into_option(self) -> Option<Self> {
        let has_id = self.id.is_some();
        let has_classes = !self.classes.is_empty();
        let has_attrs = !self.attrs.is_empty();
        if has_id || has_classes || has_attrs {
            Some(self)
        } else {
            None
        }
    }

    // Format as a Markdown attribute block suffix, e.g. "{#id .class key=value}".
    fn to_suffix(&self) -> Option<String> {
        let mut parts = Vec::new();

        if let Some(id) = &self.id {
            parts.push(format!("#{}", id));
        }

        for class in &self.classes {
            parts.push(format!(".{}", class));
        }

        for (key, value) in &self.attrs {
            match value {
                Some(value) => parts.push(format!("{}={}", key, value)),
                None => parts.push(key.to_string()),
            }
        }

        if parts.is_empty() {
            None
        } else {
            Some(format!(" {{{}}}", parts.join(" ")))
        }
    }
}

#[derive(Default)]
struct TableCell<'a> {
    spans: Vec<Span<'a>>,
}

#[derive(Default)]
struct TableRow<'a> {
    cells: Vec<TableCell<'a>>,
    is_header: bool,
}

#[derive(Default)]
struct TableState<'a> {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow<'a>>,
    current_row: Option<TableRow<'a>>,
    current_cell: Option<TableCell<'a>>,
    in_head: bool,
}

impl<'a> TableState<'a> {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            ..Self::default()
        }
    }

    fn start_head(&mut self) {
        self.in_head = true;
    }

    fn end_head(&mut self) {
        self.end_row();
        self.in_head = false;
    }

    fn start_row(&mut self) {
        self.current_row = Some(TableRow {
            is_header: self.in_head,
            ..TableRow::default()
        });
    }

    fn end_row(&mut self) {
        self.end_cell();
        if let Some(row) = self.current_row.take() {
            self.rows.push(row);
        }
    }

    fn start_cell(&mut self) {
        if self.current_row.is_none() {
            self.start_row();
        }
        self.current_cell = Some(TableCell::default());
    }

    fn end_cell(&mut self) {
        let Some(cell) = self.current_cell.take() else {
            return;
        };
        if let Some(row) = self.current_row.as_mut() {
            row.cells.push(cell);
        }
    }

    fn push_span(&mut self, span: Span<'a>) {
        if let Some(cell) = self.current_cell.as_mut() {
            cell.spans.push(span);
        }
    }

    fn into_lines(self, header_style: Style) -> Vec<Line<'a>> {
        let column_count = self.alignments.len().max(
            self.rows
                .iter()
                .map(|row| row.cells.len())
                .max()
                .unwrap_or(0),
        );

        if column_count == 0 || self.rows.is_empty() {
            return Vec::new();
        }

        let mut widths = vec![1; column_count];
        for row in &self.rows {
            for (column, cell) in row.cells.iter().enumerate() {
                widths[column] = widths[column].max(display_width_spans(&cell.spans));
            }
        }

        let mut lines = Vec::with_capacity(self.rows.len() * 2 + 1);
        lines.push(build_table_border(&widths, "┌", "┬", "┐"));

        for (index, row) in self.rows.iter().enumerate() {
            lines.push(build_table_row(
                row,
                &self.alignments,
                &widths,
                header_style,
            ));

            if index + 1 < self.rows.len() {
                lines.push(build_table_border(&widths, "├", "┼", "┤"));
            }
        }

        lines.push(build_table_border(&widths, "└", "┴", "┘"));
        lines
    }
}

fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

fn display_width_spans(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn build_table_border<'a>(
    widths: &[usize],
    left: &'static str,
    mid: &'static str,
    right: &'static str,
) -> Line<'a> {
    let mut spans = Vec::with_capacity(widths.len() * 2 + 1);
    spans.push(Span::from(left));
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::from("─".repeat(*width + 2)));
        spans.push(Span::from(if index + 1 == widths.len() {
            right
        } else {
            mid
        }));
    }
    Line::from(spans)
}

fn build_table_row<'a>(
    row: &TableRow<'a>,
    alignments: &[Alignment],
    widths: &[usize],
    header_style: Style,
) -> Line<'a> {
    let mut spans = Vec::with_capacity(widths.len() * 5 + 1);
    spans.push(Span::from("│"));

    for (index, width) in widths.iter().enumerate() {
        let alignment = alignments.get(index).copied().unwrap_or(Alignment::None);
        let (mut cell_spans, cell_width) = row
            .cells
            .get(index)
            .map(|cell| (cell.spans.clone(), display_width_spans(&cell.spans)))
            .unwrap_or_default();
        if row.is_header {
            for span in &mut cell_spans {
                span.style = header_style.patch(span.style);
            }
        }
        let remaining = width.saturating_sub(cell_width);
        let (left_padding, right_padding) = match alignment {
            Alignment::Right => (remaining, 0),
            Alignment::Center => (remaining / 2, remaining - (remaining / 2)),
            Alignment::None | Alignment::Left => (0, remaining),
        };

        spans.push(Span::from(" "));
        if left_padding > 0 {
            spans.push(Span::from(" ".repeat(left_padding)));
        }
        spans.append(&mut cell_spans);
        if right_padding > 0 {
            spans.push(Span::from(" ".repeat(right_padding)));
        }
        spans.push(Span::from(" "));
        spans.push(Span::from("│"));
    }

    Line::from(spans)
}

struct TextWriter<'a, I, S: StyleSheet> {
    /// Iterator supplying events.
    iter: I,

    /// Text to write to.
    text: Text<'a>,

    /// Current style.
    ///
    /// This is a stack of styles, with the top style being the current style.
    inline_styles: Vec<Style>,

    /// Prefix to add to the start of the each line.
    line_prefixes: Vec<Span<'a>>,

    /// Stack of line styles.
    line_styles: Vec<Style>,

    /// Used to highlight code blocks, set when a codeblock is encountered.
    #[cfg(feature = "highlight-code")]
    code_highlighter: Option<HighlightLines<'a>>,

    /// Current list index as a stack of indices.
    list_indices: Vec<Option<u64>>,

    /// A link which will be appended to the current line when the link tag is closed.
    link: Option<CowStr<'a>>,

    /// The [`StyleSheet`] to use to style the output.
    styles: S,

    /// Heading attributes to append after heading content.
    heading_meta: Option<HeadingMeta<'a>>,

    /// Whether we are inside a metadata block.
    in_metadata_block: bool,

    /// Track if we need a newline after the current line.
    needs_newline: bool,

    /// Track if we just added a list item bullet/number.
    just_added_list_item: bool,

    /// Buffered table state. Tables need a full pass to compute column widths.
    table: Option<TableState<'a>>,
}

#[cfg(feature = "highlight-code")]
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
#[cfg(feature = "highlight-code")]
static THEME_SET: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

impl<'a, I, S> TextWriter<'a, I, S>
where
    I: Iterator<Item = Event<'a>>,
    S: StyleSheet,
{
    fn new(iter: I, styles: S) -> Self {
        Self {
            iter,
            text: Text::default(),
            inline_styles: vec![],
            line_styles: vec![],
            line_prefixes: vec![],
            list_indices: vec![],
            needs_newline: false,
            just_added_list_item: false,
            #[cfg(feature = "highlight-code")]
            code_highlighter: None,
            link: None,
            styles,
            heading_meta: None,
            in_metadata_block: false,
            table: None,
        }
    }

    fn run(&mut self) {
        debug!("Running text writer");
        while let Some(event) = self.iter.next() {
            self.handle_event(event);
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn handle_event(&mut self, event: Event<'a>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(text),
            Event::Code(code) => self.code(code),
            Event::Html(_) => warn!("Html not yet supported"),
            Event::InlineHtml(_) => warn!("Inline html not yet supported"),
            Event::FootnoteReference(_) => warn!("Footnote reference not yet supported"),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => self.task_list_marker(checked),
            Event::InlineMath(_) => warn!("Inline math not yet supported"),
            Event::DisplayMath(_) => warn!("Display math not yet supported"),
        }
    }

    fn start_tag(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::Paragraph => self.start_paragraph(),
            Tag::Heading {
                level,
                id,
                classes,
                attrs,
            } => self.start_heading(level, HeadingMeta { id, classes, attrs }),
            Tag::BlockQuote(kind) => self.start_blockquote(kind),
            Tag::CodeBlock(kind) => self.start_codeblock(kind),
            Tag::HtmlBlock => warn!("Html block not yet supported"),
            Tag::List(start_index) => self.start_list(start_index),
            Tag::Item => self.start_item(),
            Tag::FootnoteDefinition(_) => warn!("Footnote definition not yet supported"),
            Tag::Table(alignments) => self.start_table(alignments),
            Tag::TableHead => self.start_table_head(),
            Tag::TableRow => self.start_table_row(),
            Tag::TableCell => self.start_table_cell(),
            Tag::Emphasis => self.push_inline_style(Style::new().italic()),
            Tag::Strong => self.push_inline_style(Style::new().bold()),
            Tag::Strikethrough => self.push_inline_style(Style::new().crossed_out()),
            Tag::Subscript => self.push_inline_style(Style::new().dim().italic()),
            Tag::Superscript => self.push_inline_style(Style::new().dim().italic()),
            Tag::Link { dest_url, .. } => self.push_link(dest_url),
            Tag::Image { .. } => warn!("Image not yet supported"),
            Tag::MetadataBlock(_) => self.start_metadata_block(),
            Tag::DefinitionList => warn!("Definition list not yet supported"),
            Tag::DefinitionListTitle => warn!("Definition list title not yet supported"),
            Tag::DefinitionListDefinition => warn!("Definition list definition not yet supported"),
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_paragraph(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote(_) => self.end_blockquote(),
            TagEnd::CodeBlock => self.end_codeblock(),
            TagEnd::HtmlBlock => {}
            TagEnd::List(_is_ordered) => self.end_list(),
            TagEnd::Item => {}
            TagEnd::FootnoteDefinition => {}
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead => self.end_table_head(),
            TagEnd::TableRow => self.end_table_row(),
            TagEnd::TableCell => self.end_table_cell(),
            TagEnd::Emphasis => self.pop_inline_style(),
            TagEnd::Strong => self.pop_inline_style(),
            TagEnd::Strikethrough => self.pop_inline_style(),
            TagEnd::Subscript => self.pop_inline_style(),
            TagEnd::Superscript => self.pop_inline_style(),
            TagEnd::Link => self.pop_link(),
            TagEnd::Image => {}
            TagEnd::MetadataBlock(_) => self.end_metadata_block(),
            TagEnd::DefinitionList => {}
            TagEnd::DefinitionListTitle => {}
            TagEnd::DefinitionListDefinition => {}
        }
    }

    fn start_paragraph(&mut self) {
        // Insert an empty line between paragraphs if there is at least one line of text already.
        if self.needs_newline {
            self.push_line(Line::default());
        }
        // Don't create a new line if we just added a list item (bullet/number).
        if !self.just_added_list_item {
            self.push_line(Line::default());
        }
        self.needs_newline = false;
        self.just_added_list_item = false;
    }

    fn end_paragraph(&mut self) {
        self.needs_newline = true
    }

    fn start_heading(&mut self, level: HeadingLevel, heading_meta: HeadingMeta<'a>) {
        if self.needs_newline {
            self.push_line(Line::default());
        }
        let heading_level = match level {
            HeadingLevel::H1 => 1,
            HeadingLevel::H2 => 2,
            HeadingLevel::H3 => 3,
            HeadingLevel::H4 => 4,
            HeadingLevel::H5 => 5,
            HeadingLevel::H6 => 6,
        };
        let style = self.styles.heading(heading_level);
        let content = format!("{} ", "#".repeat(heading_level as usize));
        self.push_line(Line::styled(content, style));
        self.heading_meta = heading_meta.into_option();
        self.needs_newline = false;
    }

    fn end_heading(&mut self) {
        if let Some(meta) = self.heading_meta.take() {
            if let Some(suffix) = meta.to_suffix() {
                self.push_span(Span::styled(suffix, self.styles.heading_meta()));
            }
        }
        self.needs_newline = true
    }

    fn start_blockquote(&mut self, _kind: Option<BlockQuoteKind>) {
        if self.needs_newline {
            self.push_line(Line::default());
            self.needs_newline = false;
        }
        self.line_prefixes.push(Span::from(">"));
        self.line_styles.push(self.styles.blockquote());
    }

    fn end_blockquote(&mut self) {
        self.line_prefixes.pop();
        self.line_styles.pop();
        self.needs_newline = true;
    }

    fn text(&mut self, text: CowStr<'a>) {
        if self.in_table_cell() {
            let style = self.inline_styles.last().copied().unwrap_or_default();
            let normalized = text.lines().join(" ");
            if !normalized.is_empty() {
                self.push_span(Span::styled(normalized, style));
            }
            self.needs_newline = false;
            self.just_added_list_item = false;
            return;
        }

        #[cfg(feature = "highlight-code")]
        if let Some(highlighter) = &mut self.code_highlighter {
            let text: Text = LinesWithEndings::from(&text)
                .filter_map(|line| highlighter.highlight_line(line, &SYNTAX_SET).ok())
                .filter_map(|part| as_24_bit_terminal_escaped(&part, false).into_text().ok())
                .flatten()
                .collect();

            for line in text.lines {
                self.text.push_line(line);
            }
            self.needs_newline = false;
            return;
        }

        for (position, line) in text.lines().with_position() {
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if matches!(position, Position::Middle | Position::Last) {
                self.push_line(Line::default());
            }

            let style = self.inline_styles.last().copied().unwrap_or_default();

            let span = Span::styled(line.to_owned(), style);

            self.push_span(span);
        }
        self.needs_newline = false;
        self.just_added_list_item = false;
    }

    fn code(&mut self, code: CowStr<'a>) {
        let span = Span::styled(code, self.styles.code());
        self.push_span(span);
    }

    fn hard_break(&mut self) {
        if self.in_table_cell() {
            self.push_span(Span::raw(" "));
            return;
        }
        self.push_line(Line::default());
    }

    fn start_metadata_block(&mut self) {
        if self.needs_newline {
            self.push_line(Line::default());
        }
        self.line_styles.push(self.styles.metadata_block());
        self.push_line(Line::from("---"));
        self.push_line(Line::default());
        self.in_metadata_block = true;
    }

    fn end_metadata_block(&mut self) {
        if self.in_metadata_block {
            self.push_line(Line::from("---"));
            self.line_styles.pop();
            self.in_metadata_block = false;
            self.needs_newline = true;
        }
    }

    fn rule(&mut self) {
        if self.needs_newline {
            self.push_line(Line::default());
        }
        self.push_line(Line::from("---"));
        self.needs_newline = true;
    }

    fn start_list(&mut self, index: Option<u64>) {
        if self.list_indices.is_empty() && self.needs_newline {
            self.push_line(Line::default());
        }
        self.list_indices.push(index);
    }

    fn end_list(&mut self) {
        self.list_indices.pop();
        self.needs_newline = true;
    }

    fn start_item(&mut self) {
        self.push_line(Line::default());
        let width = self.list_indices.len() * 4 - 3;
        if let Some(last_index) = self.list_indices.last_mut() {
            let span = match last_index {
                None => Span::from(" ".repeat(width - 1) + "- "),
                Some(index) => {
                    *index += 1;
                    format!("{:width$}. ", *index - 1).light_blue()
                }
            };
            self.push_span(span);
        }
        self.needs_newline = false;
        self.just_added_list_item = true;
    }

    fn task_list_marker(&mut self, checked: bool) {
        let marker = if checked { 'x' } else { ' ' };
        let marker_span = Span::from(format!("[{}] ", marker));
        if let Some(line) = self.text.lines.last_mut() {
            if let Some(first_span) = line.spans.first_mut() {
                let content = first_span.content.to_mut();
                if content.ends_with("- ") {
                    let len = content.len();
                    content.truncate(len - 2);
                    content.push_str("- [");
                    content.push(marker);
                    content.push_str("] ");
                    return;
                }
            }
            line.spans.insert(1, marker_span);
        } else {
            self.push_span(marker_span);
        }
    }

    fn soft_break(&mut self) {
        if self.in_table_cell() {
            self.push_span(Span::raw(" "));
            return;
        }
        if self.in_metadata_block {
            self.hard_break();
        } else {
            self.push_span(Span::raw(" "));
        }
    }

    fn start_table(&mut self, alignments: Vec<Alignment>) {
        if self.needs_newline {
            self.push_line(Line::default());
        }
        self.table = Some(TableState::new(alignments));
        self.needs_newline = false;
        self.just_added_list_item = false;
    }

    fn end_table(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.end_row();
        }
        let Some(table) = self.table.take() else {
            return;
        };
        let header_style = self.styles.table_header();
        for line in table.into_lines(header_style) {
            self.push_line(line);
        }
        self.needs_newline = true;
        self.just_added_list_item = false;
    }

    fn start_table_head(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.start_head();
        }
    }

    fn end_table_head(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.end_head();
        }
    }

    fn start_table_row(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.start_row();
        }
    }

    fn end_table_row(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.end_row();
        }
    }

    fn start_table_cell(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.start_cell();
        }
    }

    fn end_table_cell(&mut self) {
        if let Some(table) = self.table.as_mut() {
            table.end_cell();
        }
    }

    fn in_table_cell(&self) -> bool {
        self.table
            .as_ref()
            .is_some_and(|table| table.current_cell.is_some())
    }

    fn start_codeblock(&mut self, kind: CodeBlockKind<'_>) {
        if !self.text.lines.is_empty() {
            self.push_line(Line::default());
        }
        let lang = match kind {
            CodeBlockKind::Fenced(ref lang) => lang.as_ref(),
            CodeBlockKind::Indented => "",
        };

        #[cfg(not(feature = "highlight-code"))]
        self.line_styles.push(self.styles.code());

        #[cfg(feature = "highlight-code")]
        self.set_code_highlighter(lang);

        let span = Span::from(format!("```{lang}"));
        self.push_line(span.into());
        self.needs_newline = true;
    }

    fn end_codeblock(&mut self) {
        let span = Span::from("```");
        self.push_line(span.into());
        self.needs_newline = true;

        #[cfg(not(feature = "highlight-code"))]
        self.line_styles.pop();

        #[cfg(feature = "highlight-code")]
        self.clear_code_highlighter();
    }

    #[cfg(feature = "highlight-code")]
    #[instrument(level = "trace", skip(self))]
    fn set_code_highlighter(&mut self, lang: &str) {
        if let Some(syntax) = SYNTAX_SET.find_syntax_by_token(lang) {
            debug!("Starting code block with syntax: {:?}", lang);
            let theme = &THEME_SET.themes["base16-ocean.dark"];
            let highlighter = HighlightLines::new(syntax, theme);
            self.code_highlighter = Some(highlighter);
        } else {
            warn!("Could not find syntax for code block: {:?}", lang);
        }
    }

    #[cfg(feature = "highlight-code")]
    #[instrument(level = "trace", skip(self))]
    fn clear_code_highlighter(&mut self) {
        self.code_highlighter = None;
    }

    #[instrument(level = "trace", skip(self))]
    fn push_inline_style(&mut self, style: Style) {
        let current_style = self.inline_styles.last().copied().unwrap_or_default();
        let style = current_style.patch(style);
        self.inline_styles.push(style);
        debug!("Pushed inline style: {:?}", style);
        debug!("Current inline styles: {:?}", self.inline_styles);
    }

    #[instrument(level = "trace", skip(self))]
    fn pop_inline_style(&mut self) {
        self.inline_styles.pop();
    }

    #[instrument(level = "trace", skip(self))]
    fn push_line(&mut self, line: Line<'a>) {
        let style = self.line_styles.last().copied().unwrap_or_default();
        let mut line = line.patch_style(style);

        // Add line prefixes to the start of the line.
        let line_prefixes = self.line_prefixes.iter().cloned().collect_vec();
        let has_prefixes = !line_prefixes.is_empty();
        if has_prefixes {
            line.spans.insert(0, " ".into());
        }
        for prefix in line_prefixes.iter().rev().cloned() {
            line.spans.insert(0, prefix);
        }
        self.text.lines.push(line);
    }

    #[instrument(level = "trace", skip(self))]
    fn push_span(&mut self, span: Span<'a>) {
        if let Some(table) = self.table.as_mut() {
            if table.current_cell.is_some() {
                table.push_span(span);
                return;
            }
        }

        if let Some(line) = self.text.lines.last_mut() {
            line.push_span(span);
        } else {
            self.push_line(Line::from(vec![span]));
        }
    }

    /// Store the link to be appended to the link text
    #[instrument(level = "trace", skip(self))]
    fn push_link(&mut self, dest_url: CowStr<'a>) {
        self.link = Some(dest_url);
    }

    /// Append the link to the current line
    #[instrument(level = "trace", skip(self))]
    fn pop_link(&mut self) {
        if let Some(link) = self.link.take() {
            self.push_span(" (".into());
            self.push_span(Span::styled(link, self.styles.link()));
            self.push_span(")".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use pretty_assertions::assert_eq;
    use rstest::{fixture, rstest};
    use tracing::level_filters::LevelFilter;
    use tracing::subscriber::{self, DefaultGuard};
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::fmt::time::Uptime;

    use super::*;

    #[fixture]
    fn with_tracing() -> DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_test_writer()
            .with_timer(Uptime::default())
            .with_max_level(LevelFilter::TRACE)
            .with_span_events(FmtSpan::ENTER)
            .finish();
        subscriber::set_default(subscriber)
    }

    #[rstest]
    fn empty(_with_tracing: DefaultGuard) {
        assert_eq!(from_str(""), Text::default());
    }

    #[rstest]
    fn paragraph_single(_with_tracing: DefaultGuard) {
        assert_eq!(from_str("Hello, world!"), Text::from("Hello, world!"));
    }

    #[rstest]
    fn paragraph_soft_break(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                Hello
                World
            "}),
            Text::from(Line::from_iter([
                Span::from("Hello"),
                Span::from(" "),
                Span::from("World"),
            ]))
        );
    }

    #[rstest]
    fn paragraph_multiple(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                Paragraph 1

                Paragraph 2
            "}),
            Text::from_iter(["Paragraph 1", "", "Paragraph 2",])
        );
    }

    #[rstest]
    fn rule(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                Paragraph 1

                ---

                Paragraph 2
            "}),
            Text::from_iter(["Paragraph 1", "", "---", "", "Paragraph 2"])
        );
    }

    #[rstest]
    fn headings(_with_tracing: DefaultGuard) {
        let h1 = Style::new().on_cyan().bold().underlined();
        let h2 = Style::new().cyan().bold();
        let h3 = Style::new().cyan().bold().italic();
        let h4 = Style::new().light_cyan().italic();
        let h5 = Style::new().light_cyan().italic();
        let h6 = Style::new().light_cyan().italic();

        assert_eq!(
            from_str(indoc! {"
                # Heading 1
                ## Heading 2
                ### Heading 3
                #### Heading 4
                ##### Heading 5
                ###### Heading 6
            "}),
            Text::from_iter([
                Line::from_iter(["# ", "Heading 1"]).style(h1),
                Line::default(),
                Line::from_iter(["## ", "Heading 2"]).style(h2),
                Line::default(),
                Line::from_iter(["### ", "Heading 3"]).style(h3),
                Line::default(),
                Line::from_iter(["#### ", "Heading 4"]).style(h4),
                Line::default(),
                Line::from_iter(["##### ", "Heading 5"]).style(h5),
                Line::default(),
                Line::from_iter(["###### ", "Heading 6"]).style(h6),
            ])
        );
    }

    #[rstest]
    fn heading_attributes(_with_tracing: DefaultGuard) {
        let h1 = Style::new().on_cyan().bold().underlined();
        let meta = Style::new().dim();

        assert_eq!(
            from_str("# Heading {#title .primary data-kind=doc}"),
            Text::from(
                Line::from_iter([
                    Span::from("# "),
                    Span::from("Heading"),
                    Span::styled(" {#title .primary data-kind=doc}", meta),
                ])
                .style(h1)
            )
        );
    }

    mod blockquote {
        use pretty_assertions::assert_eq;
        use ratatui::style::Color;

        use super::*;

        const STYLE: Style = Style::new().fg(Color::Green);

        /// I was having difficulty getting the right number of newlines between paragraphs, so this
        /// test is to help debug and ensure that.
        #[rstest]
        fn after_paragraph(_with_tracing: DefaultGuard) {
            assert_eq!(
                from_str(indoc! {"
                Hello, world!

                > Blockquote
            "}),
                Text::from_iter([
                    Line::from("Hello, world!"),
                    Line::default(),
                    Line::from_iter([">", " ", "Blockquote"]).style(STYLE),
                ])
            );
        }
        #[rstest]
        fn single(_with_tracing: DefaultGuard) {
            assert_eq!(
                from_str("> Blockquote"),
                Text::from(Line::from_iter([">", " ", "Blockquote"]).style(STYLE))
            );
        }

        #[rstest]
        fn soft_break(_with_tracing: DefaultGuard) {
            assert_eq!(
                from_str(indoc! {"
                > Blockquote 1
                > Blockquote 2
            "}),
                Text::from(
                    Line::from_iter([">", " ", "Blockquote 1", " ", "Blockquote 2"]).style(STYLE)
                )
            );
        }

        #[rstest]
        fn multiple(_with_tracing: DefaultGuard) {
            assert_eq!(
                from_str(indoc! {"
                > Blockquote 1
                >
                > Blockquote 2
            "}),
                Text::from_iter([
                    Line::from_iter([">", " ", "Blockquote 1"]).style(STYLE),
                    Line::from_iter([">", " "]).style(STYLE),
                    Line::from_iter([">", " ", "Blockquote 2"]).style(STYLE),
                ])
            );
        }

        #[rstest]
        fn multiple_with_break(_with_tracing: DefaultGuard) {
            assert_eq!(
                from_str(indoc! {"
                > Blockquote 1

                > Blockquote 2
            "}),
                Text::from_iter([
                    Line::from_iter([">", " ", "Blockquote 1"]).style(STYLE),
                    Line::default(),
                    Line::from_iter([">", " ", "Blockquote 2"]).style(STYLE),
                ])
            );
        }

        #[rstest]
        fn nested(_with_tracing: DefaultGuard) {
            assert_eq!(
                from_str(indoc! {"
                > Blockquote 1
                >> Nested Blockquote
            "}),
                Text::from_iter([
                    Line::from_iter([">", " ", "Blockquote 1"]).style(STYLE),
                    Line::from_iter([">", " "]).style(STYLE),
                    Line::from_iter([">", ">", " ", "Nested Blockquote"]).style(STYLE),
                ])
            );
        }
    }

    #[rstest]
    fn list_single(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                - List item 1
            "}),
            Text::from_iter([Line::from_iter(["- ", "List item 1"])])
        );
    }

    #[rstest]
    fn list_multiple(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                - List item 1
                - List item 2
            "}),
            Text::from_iter([
                Line::from_iter(["- ", "List item 1"]),
                Line::from_iter(["- ", "List item 2"]),
            ])
        );
    }

    #[rstest]
    fn list_ordered(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                1. List item 1
                2. List item 2
            "}),
            Text::from_iter([
                Line::from_iter(["1. ".light_blue(), "List item 1".into()]),
                Line::from_iter(["2. ".light_blue(), "List item 2".into()]),
            ])
        );
    }

    #[rstest]
    fn list_nested(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                - List item 1
                  - Nested list item 1
            "}),
            Text::from_iter([
                Line::from_iter(["- ", "List item 1"]),
                Line::from_iter(["    - ", "Nested list item 1"]),
            ])
        );
    }

    #[rstest]
    fn list_task_items(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                - [ ] Incomplete
                - [x] Complete
            "}),
            Text::from_iter([
                Line::from_iter(["- [ ] ", "Incomplete"]),
                Line::from_iter(["- [x] ", "Complete"]),
            ])
        );
    }

    #[rstest]
    fn list_task_items_ordered(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                1. [ ] Incomplete
                2. [x] Complete
            "}),
            Text::from_iter([
                Line::from_iter(["1. ".light_blue(), "[ ] ".into(), "Incomplete".into(),]),
                Line::from_iter(["2. ".light_blue(), "[x] ".into(), "Complete".into(),]),
            ])
        );
    }

    #[cfg_attr(not(feature = "highlight-code"), ignore)]
    #[rstest]
    fn highlighted_code(_with_tracing: DefaultGuard) {
        // Assert no extra newlines are added
        let highlighted_code = from_str(indoc! {"
            ```rust
            fn main() {
                println!(\"Hello, highlighted code!\");
            }
            ```"});

        insta::assert_snapshot!(highlighted_code);
        insta::assert_debug_snapshot!(highlighted_code);
    }

    #[cfg_attr(not(feature = "highlight-code"), ignore)]
    #[rstest]
    fn highlighted_code_with_indentation(_with_tracing: DefaultGuard) {
        // Assert no extra newlines are added
        let highlighted_code_indented = from_str(indoc! {"
            ```rust
            fn main() {
                // This is a comment
                HelloWorldBuilder::new()
                    .with_text(\"Hello, highlighted code!\")
                    .build()
                    .show();

            }
            ```"});

        insta::assert_snapshot!(highlighted_code_indented);
        insta::assert_debug_snapshot!(highlighted_code_indented);
    }

    #[cfg_attr(feature = "highlight-code", ignore)]
    #[rstest]
    fn unhighlighted_code(_with_tracing: DefaultGuard) {
        // Assert no extra newlines are added
        let unhiglighted_code = from_str(indoc! {"
            ```rust
            fn main() {
                println!(\"Hello, unhighlighted code!\");
            }
            ```"});

        insta::assert_snapshot!(unhiglighted_code);

        // Code highlighting is complex, assert on on the debug snapshot
        insta::assert_debug_snapshot!(unhiglighted_code);
    }

    #[rstest]
    fn inline_code(_with_tracing: DefaultGuard) {
        let text = from_str("Example of `Inline code`");
        insta::assert_snapshot!(text);

        assert_eq!(
            text,
            Line::from_iter([
                Span::from("Example of "),
                Span::styled("Inline code", Style::new().white().on_black())
            ])
            .into()
        );
    }

    #[rstest]
    fn superscript(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("H ^2^ O"),
            Text::from(Line::from_iter([
                Span::from("H "),
                Span::styled("2", Style::new().dim().italic()),
                Span::from(" O"),
            ]))
        );
    }

    #[rstest]
    fn subscript(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("H ~2~ O"),
            Text::from(Line::from_iter([
                Span::from("H "),
                Span::styled("2", Style::new().dim().italic()),
                Span::from(" O"),
            ]))
        );
    }

    #[rstest]
    fn metadata_block(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str(indoc! {"
                ---
                title: Demo
                ---

                Body
            "}),
            Text::from_iter([
                Line::from("---").style(Style::new().light_yellow()),
                Line::from("title: Demo").style(Style::new().light_yellow()),
                Line::from("---").style(Style::new().light_yellow()),
                Line::default(),
                Line::from("Body"),
            ])
        );
    }

    #[rstest]
    fn strong(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("**Strong**"),
            Text::from(Line::from("Strong".bold()))
        );
    }

    #[rstest]
    fn emphasis(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("*Emphasis*"),
            Text::from(Line::from("Emphasis".italic()))
        );
    }

    #[rstest]
    fn strikethrough(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("~~Strikethrough~~"),
            Text::from(Line::from("Strikethrough".crossed_out()))
        );
    }

    #[rstest]
    fn strong_emphasis(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("**Strong *emphasis***"),
            Text::from(Line::from_iter([
                "Strong ".bold(),
                "emphasis".bold().italic()
            ]))
        );
    }

    #[rstest]
    fn link(_with_tracing: DefaultGuard) {
        assert_eq!(
            from_str("[Link](https://example.com)"),
            Text::from(Line::from_iter([
                Span::from("Link"),
                Span::from(" ("),
                Span::from("https://example.com").blue().underlined(),
                Span::from(")")
            ]))
        );
    }
}
