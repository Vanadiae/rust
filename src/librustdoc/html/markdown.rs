//! Markdown formatting for rustdoc.
//!
//! This module implements markdown formatting through the pulldown-cmark library.
//!
//! ```
//! #![feature(rustc_private)]
//!
//! extern crate rustc_span;
//!
//! use rustc_span::edition::Edition;
//! use rustdoc::html::markdown::{HeadingOffset, IdMap, Markdown, ErrorCodes};
//!
//! let s = "My *markdown* _text_";
//! let mut id_map = IdMap::new();
//! let md = Markdown {
//!     content: s,
//!     links: &[],
//!     ids: &mut id_map,
//!     error_codes: ErrorCodes::Yes,
//!     edition: Edition::Edition2015,
//!     playground: &None,
//!     heading_offset: HeadingOffset::H2,
//!     custom_code_classes_in_docs: true,
//! };
//! let html = md.into_string();
//! // ... something using html
//! ```

use rustc_data_structures::fx::FxHashMap;
use rustc_errors::{DiagnosticMessage, SubdiagnosticMessage};
use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;
pub(crate) use rustc_resolve::rustdoc::main_body_opts;
use rustc_resolve::rustdoc::may_be_doc_link;
use rustc_span::edition::Edition;
use rustc_span::{Span, Symbol};

use once_cell::sync::Lazy;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Write;
use std::iter::Peekable;
use std::ops::{ControlFlow, Range};
use std::str::{self, CharIndices};

use crate::clean::RenderedLink;
use crate::doctest;
use crate::html::escape::Escape;
use crate::html::format::Buffer;
use crate::html::highlight;
use crate::html::length_limit::HtmlWithLimit;
use crate::html::render::small_url_encode;
use crate::html::toc::TocBuilder;

use pulldown_cmark::{
    html, BrokenLink, CodeBlockKind, CowStr, Event, LinkType, OffsetIter, Options, Parser, Tag,
};

#[cfg(test)]
mod tests;

const MAX_HEADER_LEVEL: u32 = 6;

/// Options for rendering Markdown in summaries (e.g., in search results).
pub(crate) fn summary_opts() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_SMART_PUNCTUATION
}

#[derive(Debug, Clone, Copy)]
pub enum HeadingOffset {
    H1 = 0,
    H2,
    H3,
    H4,
    H5,
    H6,
}

/// When `to_string` is called, this struct will emit the HTML corresponding to
/// the rendered version of the contained markdown string.
pub struct Markdown<'a> {
    pub content: &'a str,
    /// A list of link replacements.
    pub links: &'a [RenderedLink],
    /// The current list of used header IDs.
    pub ids: &'a mut IdMap,
    /// Whether to allow the use of explicit error codes in doctest lang strings.
    pub error_codes: ErrorCodes,
    /// Default edition to use when parsing doctests (to add a `fn main`).
    pub edition: Edition,
    pub playground: &'a Option<Playground>,
    /// Offset at which we render headings.
    /// E.g. if `heading_offset: HeadingOffset::H2`, then `# something` renders an `<h2>`.
    pub heading_offset: HeadingOffset,
    /// `true` if the `custom_code_classes_in_docs` feature is enabled.
    pub custom_code_classes_in_docs: bool,
}
/// A struct like `Markdown` that renders the markdown with a table of contents.
pub(crate) struct MarkdownWithToc<'a> {
    pub(crate) content: &'a str,
    pub(crate) ids: &'a mut IdMap,
    pub(crate) error_codes: ErrorCodes,
    pub(crate) edition: Edition,
    pub(crate) playground: &'a Option<Playground>,
    /// `true` if the `custom_code_classes_in_docs` feature is enabled.
    pub(crate) custom_code_classes_in_docs: bool,
}
/// A tuple struct like `Markdown` that renders the markdown escaping HTML tags
/// and includes no paragraph tags.
pub(crate) struct MarkdownItemInfo<'a>(pub(crate) &'a str, pub(crate) &'a mut IdMap);
/// A tuple struct like `Markdown` that renders only the first paragraph.
pub(crate) struct MarkdownSummaryLine<'a>(pub &'a str, pub &'a [RenderedLink]);

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum ErrorCodes {
    Yes,
    No,
}

impl ErrorCodes {
    pub(crate) fn from(b: bool) -> Self {
        match b {
            true => ErrorCodes::Yes,
            false => ErrorCodes::No,
        }
    }

    pub(crate) fn as_bool(self) -> bool {
        match self {
            ErrorCodes::Yes => true,
            ErrorCodes::No => false,
        }
    }
}

/// Controls whether a line will be hidden or shown in HTML output.
///
/// All lines are used in documentation tests.
enum Line<'a> {
    Hidden(&'a str),
    Shown(Cow<'a, str>),
}

impl<'a> Line<'a> {
    fn for_html(self) -> Option<Cow<'a, str>> {
        match self {
            Line::Shown(l) => Some(l),
            Line::Hidden(_) => None,
        }
    }

    fn for_code(self) -> Cow<'a, str> {
        match self {
            Line::Shown(l) => l,
            Line::Hidden(l) => Cow::Borrowed(l),
        }
    }
}

// FIXME: There is a minor inconsistency here. For lines that start with ##, we
// have no easy way of removing a potential single space after the hashes, which
// is done in the single # case. This inconsistency seems okay, if non-ideal. In
// order to fix it we'd have to iterate to find the first non-# character, and
// then reallocate to remove it; which would make us return a String.
fn map_line(s: &str) -> Line<'_> {
    let trimmed = s.trim();
    if trimmed.starts_with("##") {
        Line::Shown(Cow::Owned(s.replacen("##", "#", 1)))
    } else if let Some(stripped) = trimmed.strip_prefix("# ") {
        // # text
        Line::Hidden(stripped)
    } else if trimmed == "#" {
        // We cannot handle '#text' because it could be #[attr].
        Line::Hidden("")
    } else {
        Line::Shown(Cow::Borrowed(s))
    }
}

/// Convert chars from a title for an id.
///
/// "Hello, world!" -> "hello-world"
fn slugify(c: char) -> Option<char> {
    if c.is_alphanumeric() || c == '-' || c == '_' {
        if c.is_ascii() { Some(c.to_ascii_lowercase()) } else { Some(c) }
    } else if c.is_whitespace() && c.is_ascii() {
        Some('-')
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub struct Playground {
    pub crate_name: Option<Symbol>,
    pub url: String,
}

/// Adds syntax highlighting and playground Run buttons to Rust code blocks.
struct CodeBlocks<'p, 'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    check_error_codes: ErrorCodes,
    edition: Edition,
    // Information about the playground if a URL has been specified, containing an
    // optional crate name and the URL.
    playground: &'p Option<Playground>,
    custom_code_classes_in_docs: bool,
}

impl<'p, 'a, I: Iterator<Item = Event<'a>>> CodeBlocks<'p, 'a, I> {
    fn new(
        iter: I,
        error_codes: ErrorCodes,
        edition: Edition,
        playground: &'p Option<Playground>,
        custom_code_classes_in_docs: bool,
    ) -> Self {
        CodeBlocks {
            inner: iter,
            check_error_codes: error_codes,
            edition,
            playground,
            custom_code_classes_in_docs,
        }
    }
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for CodeBlocks<'_, 'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let event = self.inner.next();
        let compile_fail;
        let should_panic;
        let ignore;
        let edition;
        let Some(Event::Start(Tag::CodeBlock(kind))) = event else {
            return event;
        };

        let mut original_text = String::new();
        for event in &mut self.inner {
            match event {
                Event::End(Tag::CodeBlock(..)) => break,
                Event::Text(ref s) => {
                    original_text.push_str(s);
                }
                _ => {}
            }
        }

        let parse_result = match kind {
            CodeBlockKind::Fenced(ref lang) => {
                let parse_result = LangString::parse_without_check(
                    lang,
                    self.check_error_codes,
                    false,
                    self.custom_code_classes_in_docs,
                );
                if !parse_result.rust {
                    let added_classes = parse_result.added_classes;
                    let lang_string = if let Some(lang) = parse_result.unknown.first() {
                        format!("language-{}", lang)
                    } else {
                        String::new()
                    };
                    let whitespace = if added_classes.is_empty() { "" } else { " " };
                    return Some(Event::Html(
                        format!(
                            "<div class=\"example-wrap\">\
                                 <pre class=\"{lang_string}{whitespace}{added_classes}\">\
                                     <code>{text}</code>\
                                 </pre>\
                             </div>",
                            added_classes = added_classes.join(" "),
                            text = Escape(&original_text),
                        )
                        .into(),
                    ));
                }
                parse_result
            }
            CodeBlockKind::Indented => Default::default(),
        };

        let added_classes = parse_result.added_classes;
        let lines = original_text.lines().filter_map(|l| map_line(l).for_html());
        let text = lines.intersperse("\n".into()).collect::<String>();

        compile_fail = parse_result.compile_fail;
        should_panic = parse_result.should_panic;
        ignore = parse_result.ignore;
        edition = parse_result.edition;

        let explicit_edition = edition.is_some();
        let edition = edition.unwrap_or(self.edition);

        let playground_button = self.playground.as_ref().and_then(|playground| {
            let krate = &playground.crate_name;
            let url = &playground.url;
            if url.is_empty() {
                return None;
            }
            let test = original_text
                .lines()
                .map(|l| map_line(l).for_code())
                .intersperse("\n".into())
                .collect::<String>();
            let krate = krate.as_ref().map(|s| s.as_str());
            let (test, _, _) =
                doctest::make_test(&test, krate, false, &Default::default(), edition, None);
            let channel = if test.contains("#![feature(") { "&amp;version=nightly" } else { "" };

            let test_escaped = small_url_encode(test);
            Some(format!(
                "<a class=\"test-arrow\" \
                    target=\"_blank\" \
                    href=\"{url}?code={test_escaped}{channel}&amp;edition={edition}\">Run</a>",
            ))
        });

        let tooltip = if ignore != Ignore::None {
            highlight::Tooltip::Ignore
        } else if compile_fail {
            highlight::Tooltip::CompileFail
        } else if should_panic {
            highlight::Tooltip::ShouldPanic
        } else if explicit_edition {
            highlight::Tooltip::Edition(edition)
        } else {
            highlight::Tooltip::None
        };

        // insert newline to clearly separate it from the
        // previous block so we can shorten the html output
        let mut s = Buffer::new();
        s.push('\n');

        highlight::render_example_with_highlighting(
            &text,
            &mut s,
            tooltip,
            playground_button.as_deref(),
            &added_classes,
        );
        Some(Event::Html(s.into_inner().into()))
    }
}

/// Make headings links with anchor IDs and build up TOC.
struct LinkReplacer<'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    links: &'a [RenderedLink],
    shortcut_link: Option<&'a RenderedLink>,
}

impl<'a, I: Iterator<Item = Event<'a>>> LinkReplacer<'a, I> {
    fn new(iter: I, links: &'a [RenderedLink]) -> Self {
        LinkReplacer { inner: iter, links, shortcut_link: None }
    }
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for LinkReplacer<'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut event = self.inner.next();

        // Replace intra-doc links and remove disambiguators from shortcut links (`[fn@f]`).
        match &mut event {
            // This is a shortcut link that was resolved by the broken_link_callback: `[fn@f]`
            // Remove any disambiguator.
            Some(Event::Start(Tag::Link(
                // [fn@f] or [fn@f][]
                LinkType::ShortcutUnknown | LinkType::CollapsedUnknown,
                dest,
                title,
            ))) => {
                debug!("saw start of shortcut link to {dest} with title {title}");
                // If this is a shortcut link, it was resolved by the broken_link_callback.
                // So the URL will already be updated properly.
                let link = self.links.iter().find(|&link| *link.href == **dest);
                // Since this is an external iterator, we can't replace the inner text just yet.
                // Store that we saw a link so we know to replace it later.
                if let Some(link) = link {
                    trace!("it matched");
                    assert!(self.shortcut_link.is_none(), "shortcut links cannot be nested");
                    self.shortcut_link = Some(link);
                    if title.is_empty() && !link.tooltip.is_empty() {
                        *title = CowStr::Borrowed(link.tooltip.as_ref());
                    }
                }
            }
            // Now that we're done with the shortcut link, don't replace any more text.
            Some(Event::End(Tag::Link(
                LinkType::ShortcutUnknown | LinkType::CollapsedUnknown,
                dest,
                _,
            ))) => {
                debug!("saw end of shortcut link to {dest}");
                if self.links.iter().any(|link| *link.href == **dest) {
                    assert!(self.shortcut_link.is_some(), "saw closing link without opening tag");
                    self.shortcut_link = None;
                }
            }
            // Handle backticks in inline code blocks, but only if we're in the middle of a shortcut link.
            // [`fn@f`]
            Some(Event::Code(text)) => {
                trace!("saw code {text}");
                if let Some(link) = self.shortcut_link {
                    // NOTE: this only replaces if the code block is the *entire* text.
                    // If only part of the link has code highlighting, the disambiguator will not be removed.
                    // e.g. [fn@`f`]
                    // This is a limitation from `collect_intra_doc_links`: it passes a full link,
                    // and does not distinguish at all between code blocks.
                    // So we could never be sure we weren't replacing too much:
                    // [fn@my_`f`unc] is treated the same as [my_func()] in that pass.
                    //
                    // NOTE: .get(1..len() - 1) is to strip the backticks
                    if let Some(link) = self.links.iter().find(|l| {
                        l.href == link.href
                            && Some(&**text) == l.original_text.get(1..l.original_text.len() - 1)
                    }) {
                        debug!("replacing {text} with {new_text}", new_text = link.new_text);
                        *text = CowStr::Borrowed(&link.new_text);
                    }
                }
            }
            // Replace plain text in links, but only in the middle of a shortcut link.
            // [fn@f]
            Some(Event::Text(text)) => {
                trace!("saw text {text}");
                if let Some(link) = self.shortcut_link {
                    // NOTE: same limitations as `Event::Code`
                    if let Some(link) = self
                        .links
                        .iter()
                        .find(|l| l.href == link.href && **text == *l.original_text)
                    {
                        debug!("replacing {text} with {new_text}", new_text = link.new_text);
                        *text = CowStr::Borrowed(&link.new_text);
                    }
                }
            }
            // If this is a link, but not a shortcut link,
            // replace the URL, since the broken_link_callback was not called.
            Some(Event::Start(Tag::Link(_, dest, title))) => {
                if let Some(link) = self.links.iter().find(|&link| *link.original_text == **dest) {
                    *dest = CowStr::Borrowed(link.href.as_ref());
                    if title.is_empty() && !link.tooltip.is_empty() {
                        *title = CowStr::Borrowed(link.tooltip.as_ref());
                    }
                }
            }
            // Anything else couldn't have been a valid Rust path, so no need to replace the text.
            _ => {}
        }

        // Yield the modified event
        event
    }
}

/// Wrap HTML tables into `<div>` to prevent having the doc blocks width being too big.
struct TableWrapper<'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    stored_events: VecDeque<Event<'a>>,
}

impl<'a, I: Iterator<Item = Event<'a>>> TableWrapper<'a, I> {
    fn new(iter: I) -> Self {
        Self { inner: iter, stored_events: VecDeque::new() }
    }
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for TableWrapper<'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(first) = self.stored_events.pop_front() {
            return Some(first);
        }

        let event = self.inner.next()?;

        Some(match event {
            Event::Start(Tag::Table(t)) => {
                self.stored_events.push_back(Event::Start(Tag::Table(t)));
                Event::Html(CowStr::Borrowed("<div>"))
            }
            Event::End(Tag::Table(t)) => {
                self.stored_events.push_back(Event::Html(CowStr::Borrowed("</div>")));
                Event::End(Tag::Table(t))
            }
            e => e,
        })
    }
}

type SpannedEvent<'a> = (Event<'a>, Range<usize>);

/// Make headings links with anchor IDs and build up TOC.
struct HeadingLinks<'a, 'b, 'ids, I> {
    inner: I,
    toc: Option<&'b mut TocBuilder>,
    buf: VecDeque<SpannedEvent<'a>>,
    id_map: &'ids mut IdMap,
    heading_offset: HeadingOffset,
}

impl<'a, 'b, 'ids, I> HeadingLinks<'a, 'b, 'ids, I> {
    fn new(
        iter: I,
        toc: Option<&'b mut TocBuilder>,
        ids: &'ids mut IdMap,
        heading_offset: HeadingOffset,
    ) -> Self {
        HeadingLinks { inner: iter, toc, buf: VecDeque::new(), id_map: ids, heading_offset }
    }
}

impl<'a, 'b, 'ids, I: Iterator<Item = SpannedEvent<'a>>> Iterator
    for HeadingLinks<'a, 'b, 'ids, I>
{
    type Item = SpannedEvent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.buf.pop_front() {
            return Some(e);
        }

        let event = self.inner.next();
        if let Some((Event::Start(Tag::Heading(level, _, _)), _)) = event {
            let mut id = String::new();
            for event in &mut self.inner {
                match &event.0 {
                    Event::End(Tag::Heading(..)) => break,
                    Event::Start(Tag::Link(_, _, _)) | Event::End(Tag::Link(..)) => {}
                    Event::Text(text) | Event::Code(text) => {
                        id.extend(text.chars().filter_map(slugify));
                        self.buf.push_back(event);
                    }
                    _ => self.buf.push_back(event),
                }
            }
            let id = self.id_map.derive(id);

            if let Some(ref mut builder) = self.toc {
                let mut html_header = String::new();
                html::push_html(&mut html_header, self.buf.iter().map(|(ev, _)| ev.clone()));
                let sec = builder.push(level as u32, html_header, id.clone());
                self.buf.push_front((Event::Html(format!("{sec} ").into()), 0..0));
            }

            let level =
                std::cmp::min(level as u32 + (self.heading_offset as u32), MAX_HEADER_LEVEL);
            self.buf.push_back((Event::Html(format!("</a></h{level}>").into()), 0..0));

            let start_tags = format!(
                "<h{level} id=\"{id}\">\
                    <a href=\"#{id}\">",
            );
            return Some((Event::Html(start_tags.into()), 0..0));
        }
        event
    }
}

/// Extracts just the first paragraph.
struct SummaryLine<'a, I: Iterator<Item = Event<'a>>> {
    inner: I,
    started: bool,
    depth: u32,
    skipped_tags: u32,
}

impl<'a, I: Iterator<Item = Event<'a>>> SummaryLine<'a, I> {
    fn new(iter: I) -> Self {
        SummaryLine { inner: iter, started: false, depth: 0, skipped_tags: 0 }
    }
}

fn check_if_allowed_tag(t: &Tag<'_>) -> bool {
    matches!(
        t,
        Tag::Paragraph
            | Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough
            | Tag::Link(..)
            | Tag::BlockQuote
    )
}

fn is_forbidden_tag(t: &Tag<'_>) -> bool {
    matches!(
        t,
        Tag::CodeBlock(_)
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
    )
}

impl<'a, I: Iterator<Item = Event<'a>>> Iterator for SummaryLine<'a, I> {
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.started && self.depth == 0 {
            return None;
        }
        if !self.started {
            self.started = true;
        }
        if let Some(event) = self.inner.next() {
            let mut is_start = true;
            let is_allowed_tag = match event {
                Event::Start(ref c) => {
                    if is_forbidden_tag(c) {
                        self.skipped_tags += 1;
                        return None;
                    }
                    self.depth += 1;
                    check_if_allowed_tag(c)
                }
                Event::End(ref c) => {
                    if is_forbidden_tag(c) {
                        self.skipped_tags += 1;
                        return None;
                    }
                    self.depth -= 1;
                    is_start = false;
                    check_if_allowed_tag(c)
                }
                Event::FootnoteReference(_) => {
                    self.skipped_tags += 1;
                    false
                }
                _ => true,
            };
            if !is_allowed_tag {
                self.skipped_tags += 1;
            }
            return if !is_allowed_tag {
                if is_start {
                    Some(Event::Start(Tag::Paragraph))
                } else {
                    Some(Event::End(Tag::Paragraph))
                }
            } else {
                Some(event)
            };
        }
        None
    }
}

/// Moves all footnote definitions to the end and add back links to the
/// references.
struct Footnotes<'a, I> {
    inner: I,
    footnotes: FxHashMap<String, (Vec<Event<'a>>, u16)>,
}

impl<'a, I> Footnotes<'a, I> {
    fn new(iter: I) -> Self {
        Footnotes { inner: iter, footnotes: FxHashMap::default() }
    }

    fn get_entry(&mut self, key: &str) -> &mut (Vec<Event<'a>>, u16) {
        let new_id = self.footnotes.len() + 1;
        let key = key.to_owned();
        self.footnotes.entry(key).or_insert((Vec::new(), new_id as u16))
    }
}

impl<'a, I: Iterator<Item = SpannedEvent<'a>>> Iterator for Footnotes<'a, I> {
    type Item = SpannedEvent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next() {
                Some((Event::FootnoteReference(ref reference), range)) => {
                    let entry = self.get_entry(reference);
                    let reference = format!(
                        "<sup id=\"fnref{0}\"><a href=\"#fn{0}\">{0}</a></sup>",
                        (*entry).1
                    );
                    return Some((Event::Html(reference.into()), range));
                }
                Some((Event::Start(Tag::FootnoteDefinition(def)), _)) => {
                    let mut content = Vec::new();
                    for (event, _) in &mut self.inner {
                        if let Event::End(Tag::FootnoteDefinition(..)) = event {
                            break;
                        }
                        content.push(event);
                    }
                    let entry = self.get_entry(&def);
                    (*entry).0 = content;
                }
                Some(e) => return Some(e),
                None => {
                    if !self.footnotes.is_empty() {
                        let mut v: Vec<_> = self.footnotes.drain().map(|(_, x)| x).collect();
                        v.sort_by(|a, b| a.1.cmp(&b.1));
                        let mut ret = String::from("<div class=\"footnotes\"><hr><ol>");
                        for (mut content, id) in v {
                            write!(ret, "<li id=\"fn{id}\">").unwrap();
                            let mut is_paragraph = false;
                            if let Some(&Event::End(Tag::Paragraph)) = content.last() {
                                content.pop();
                                is_paragraph = true;
                            }
                            html::push_html(&mut ret, content.into_iter());
                            write!(ret, "&nbsp;<a href=\"#fnref{id}\">↩</a>").unwrap();
                            if is_paragraph {
                                ret.push_str("</p>");
                            }
                            ret.push_str("</li>");
                        }
                        ret.push_str("</ol></div>");
                        return Some((Event::Html(ret.into()), 0..0));
                    } else {
                        return None;
                    }
                }
            }
        }
    }
}

pub(crate) fn find_testable_code<T: doctest::Tester>(
    doc: &str,
    tests: &mut T,
    error_codes: ErrorCodes,
    enable_per_target_ignores: bool,
    extra_info: Option<&ExtraInfo<'_>>,
    custom_code_classes_in_docs: bool,
) {
    find_codes(
        doc,
        tests,
        error_codes,
        enable_per_target_ignores,
        extra_info,
        false,
        custom_code_classes_in_docs,
    )
}

pub(crate) fn find_codes<T: doctest::Tester>(
    doc: &str,
    tests: &mut T,
    error_codes: ErrorCodes,
    enable_per_target_ignores: bool,
    extra_info: Option<&ExtraInfo<'_>>,
    include_non_rust: bool,
    custom_code_classes_in_docs: bool,
) {
    let mut parser = Parser::new(doc).into_offset_iter();
    let mut prev_offset = 0;
    let mut nb_lines = 0;
    let mut register_header = None;
    while let Some((event, offset)) = parser.next() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let block_info = match kind {
                    CodeBlockKind::Fenced(ref lang) => {
                        if lang.is_empty() {
                            Default::default()
                        } else {
                            LangString::parse(
                                lang,
                                error_codes,
                                enable_per_target_ignores,
                                extra_info,
                                custom_code_classes_in_docs,
                            )
                        }
                    }
                    CodeBlockKind::Indented => Default::default(),
                };
                if !include_non_rust && !block_info.rust {
                    continue;
                }

                let mut test_s = String::new();

                while let Some((Event::Text(s), _)) = parser.next() {
                    test_s.push_str(&s);
                }
                let text = test_s
                    .lines()
                    .map(|l| map_line(l).for_code())
                    .collect::<Vec<Cow<'_, str>>>()
                    .join("\n");

                nb_lines += doc[prev_offset..offset.start].lines().count();
                // If there are characters between the preceding line ending and
                // this code block, `str::lines` will return an additional line,
                // which we subtract here.
                if nb_lines != 0 && !&doc[prev_offset..offset.start].ends_with('\n') {
                    nb_lines -= 1;
                }
                let line = tests.get_line() + nb_lines + 1;
                tests.add_test(text, block_info, line);
                prev_offset = offset.start;
            }
            Event::Start(Tag::Heading(level, _, _)) => {
                register_header = Some(level as u32);
            }
            Event::Text(ref s) if register_header.is_some() => {
                let level = register_header.unwrap();
                tests.register_header(s, level);
                register_header = None;
            }
            _ => {}
        }
    }
}

pub(crate) struct ExtraInfo<'tcx> {
    def_id: DefId,
    sp: Span,
    tcx: TyCtxt<'tcx>,
}

impl<'tcx> ExtraInfo<'tcx> {
    pub(crate) fn new(tcx: TyCtxt<'tcx>, def_id: DefId, sp: Span) -> ExtraInfo<'tcx> {
        ExtraInfo { def_id, sp, tcx }
    }

    fn error_invalid_codeblock_attr(&self, msg: impl Into<DiagnosticMessage>) {
        if let Some(def_id) = self.def_id.as_local() {
            self.tcx.struct_span_lint_hir(
                crate::lint::INVALID_CODEBLOCK_ATTRIBUTES,
                self.tcx.hir().local_def_id_to_hir_id(def_id),
                self.sp,
                msg,
                |l| l,
            );
        }
    }

    fn error_invalid_codeblock_attr_with_help(
        &self,
        msg: impl Into<DiagnosticMessage>,
        help: impl Into<SubdiagnosticMessage>,
    ) {
        if let Some(def_id) = self.def_id.as_local() {
            self.tcx.struct_span_lint_hir(
                crate::lint::INVALID_CODEBLOCK_ATTRIBUTES,
                self.tcx.hir().local_def_id_to_hir_id(def_id),
                self.sp,
                msg,
                |lint| lint.help(help),
            );
        }
    }
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub(crate) struct LangString {
    pub(crate) original: String,
    pub(crate) should_panic: bool,
    pub(crate) no_run: bool,
    pub(crate) ignore: Ignore,
    pub(crate) rust: bool,
    pub(crate) test_harness: bool,
    pub(crate) compile_fail: bool,
    pub(crate) error_codes: Vec<String>,
    pub(crate) edition: Option<Edition>,
    pub(crate) added_classes: Vec<String>,
    pub(crate) unknown: Vec<String>,
}

#[derive(Eq, PartialEq, Clone, Debug)]
pub(crate) enum Ignore {
    All,
    None,
    Some(Vec<String>),
}

/// This is the parser for fenced codeblocks attributes. It implements the following eBNF:
///
/// ```eBNF
/// lang-string = *(token-list / delimited-attribute-list / comment)
///
/// bareword = LEADINGCHAR *(CHAR)
/// bareword-without-leading-char = CHAR *(CHAR)
/// quoted-string = QUOTE *(NONQUOTE) QUOTE
/// token = bareword / quoted-string
/// token-without-leading-char = bareword-without-leading-char / quoted-string
/// sep = COMMA/WS *(COMMA/WS)
/// attribute = (DOT token)/(token EQUAL token-without-leading-char)
/// attribute-list = [sep] attribute *(sep attribute) [sep]
/// delimited-attribute-list = OPEN-CURLY-BRACKET attribute-list CLOSE-CURLY-BRACKET
/// token-list = [sep] token *(sep token) [sep]
/// comment = OPEN_PAREN *(all characters) CLOSE_PAREN
///
/// OPEN_PAREN = "("
/// CLOSE_PARENT = ")"
/// OPEN-CURLY-BRACKET = "{"
/// CLOSE-CURLY-BRACKET = "}"
/// LEADINGCHAR = ALPHA | DIGIT | "_" | "-" | ":"
/// ; All ASCII punctuation except comma, quote, equals, backslash, grave (backquote) and braces.
/// ; Comma is used to separate language tokens, so it can't be used in one.
/// ; Quote is used to allow otherwise-disallowed characters in language tokens.
/// ; Equals is used to make key=value pairs in attribute blocks.
/// ; Backslash and grave are special Markdown characters.
/// ; Braces are used to start an attribute block.
/// CHAR = ALPHA | DIGIT | "_" | "-" | ":" | "." | "!" | "#" | "$" | "%" | "&" | "*" | "+" | "/" |
///        ";" | "<" | ">" | "?" | "@" | "^" | "|" | "~"
/// NONQUOTE = %x09 / %x20 / %x21 / %x23-7E ; TAB / SPACE / all printable characters except `"`
/// COMMA = ","
/// DOT = "."
/// EQUAL = "="
///
/// ALPHA = %x41-5A / %x61-7A ; A-Z / a-z
/// DIGIT = %x30-39
/// WS = %x09 / " "
/// ```
pub(crate) struct TagIterator<'a, 'tcx> {
    inner: Peekable<CharIndices<'a>>,
    data: &'a str,
    is_in_attribute_block: bool,
    extra: Option<&'a ExtraInfo<'tcx>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LangStringToken<'a> {
    LangToken(&'a str),
    ClassAttribute(&'a str),
    KeyValueAttribute(&'a str, &'a str),
}

fn is_leading_char(c: char) -> bool {
    c == '_' || c == '-' || c == ':' || c.is_ascii_alphabetic() || c.is_ascii_digit()
}
fn is_bareword_char(c: char) -> bool {
    is_leading_char(c) || ".!#$%&*+/;<>?@^|~".contains(c)
}
fn is_separator(c: char) -> bool {
    c == ' ' || c == ',' || c == '\t'
}

struct Indices {
    start: usize,
    end: usize,
}

impl<'a, 'tcx> TagIterator<'a, 'tcx> {
    pub(crate) fn new(data: &'a str, extra: Option<&'a ExtraInfo<'tcx>>) -> Self {
        Self { inner: data.char_indices().peekable(), data, is_in_attribute_block: false, extra }
    }

    fn emit_error(&self, err: impl Into<DiagnosticMessage>) {
        if let Some(extra) = self.extra {
            extra.error_invalid_codeblock_attr(err);
        }
    }

    fn skip_separators(&mut self) -> Option<usize> {
        while let Some((pos, c)) = self.inner.peek() {
            if !is_separator(*c) {
                return Some(*pos);
            }
            self.inner.next();
        }
        None
    }

    fn parse_string(&mut self, start: usize) -> Option<Indices> {
        while let Some((pos, c)) = self.inner.next() {
            if c == '"' {
                return Some(Indices { start: start + 1, end: pos });
            }
        }
        self.emit_error("unclosed quote string `\"`");
        None
    }

    fn parse_class(&mut self, start: usize) -> Option<LangStringToken<'a>> {
        while let Some((pos, c)) = self.inner.peek().copied() {
            if is_bareword_char(c) {
                self.inner.next();
            } else {
                let class = &self.data[start + 1..pos];
                if class.is_empty() {
                    self.emit_error(format!("unexpected `{c}` character after `.`"));
                    return None;
                } else if self.check_after_token() {
                    return Some(LangStringToken::ClassAttribute(class));
                } else {
                    return None;
                }
            }
        }
        let class = &self.data[start + 1..];
        if class.is_empty() {
            self.emit_error("missing character after `.`");
            None
        } else if self.check_after_token() {
            Some(LangStringToken::ClassAttribute(class))
        } else {
            None
        }
    }

    fn parse_token(&mut self, start: usize) -> Option<Indices> {
        while let Some((pos, c)) = self.inner.peek() {
            if !is_bareword_char(*c) {
                return Some(Indices { start, end: *pos });
            }
            self.inner.next();
        }
        self.emit_error("unexpected end");
        None
    }

    fn parse_key_value(&mut self, c: char, start: usize) -> Option<LangStringToken<'a>> {
        let key_indices =
            if c == '"' { self.parse_string(start)? } else { self.parse_token(start)? };
        if key_indices.start == key_indices.end {
            self.emit_error("unexpected empty string as key");
            return None;
        }

        if let Some((_, c)) = self.inner.next() {
            if c != '=' {
                self.emit_error(format!("expected `=`, found `{}`", c));
                return None;
            }
        } else {
            self.emit_error("unexpected end");
            return None;
        }
        let value_indices = match self.inner.next() {
            Some((pos, '"')) => self.parse_string(pos)?,
            Some((pos, c)) if is_bareword_char(c) => self.parse_token(pos)?,
            Some((_, c)) => {
                self.emit_error(format!("unexpected `{c}` character after `=`"));
                return None;
            }
            None => {
                self.emit_error("expected value after `=`");
                return None;
            }
        };
        if value_indices.start == value_indices.end {
            self.emit_error("unexpected empty string as value");
            None
        } else if self.check_after_token() {
            Some(LangStringToken::KeyValueAttribute(
                &self.data[key_indices.start..key_indices.end],
                &self.data[value_indices.start..value_indices.end],
            ))
        } else {
            None
        }
    }

    /// Returns `false` if an error was emitted.
    fn check_after_token(&mut self) -> bool {
        if let Some((_, c)) = self.inner.peek().copied() {
            if c == '}' || is_separator(c) || c == '(' {
                true
            } else {
                self.emit_error(format!("unexpected `{c}` character"));
                false
            }
        } else {
            // The error will be caught on the next iteration.
            true
        }
    }

    fn parse_in_attribute_block(&mut self) -> Option<LangStringToken<'a>> {
        while let Some((pos, c)) = self.inner.next() {
            if c == '}' {
                self.is_in_attribute_block = false;
                return self.next();
            } else if c == '.' {
                return self.parse_class(pos);
            } else if c == '"' || is_leading_char(c) {
                return self.parse_key_value(c, pos);
            } else {
                self.emit_error(format!("unexpected character `{c}`"));
                return None;
            }
        }
        self.emit_error("unclosed attribute block (`{}`): missing `}` at the end");
        None
    }

    /// Returns `false` if an error was emitted.
    fn skip_paren_block(&mut self) -> bool {
        while let Some((_, c)) = self.inner.next() {
            if c == ')' {
                return true;
            }
        }
        self.emit_error("unclosed comment: missing `)` at the end");
        false
    }

    fn parse_outside_attribute_block(&mut self, start: usize) -> Option<LangStringToken<'a>> {
        while let Some((pos, c)) = self.inner.next() {
            if c == '"' {
                if pos != start {
                    self.emit_error("expected ` `, `{` or `,` found `\"`");
                    return None;
                }
                let indices = self.parse_string(pos)?;
                if let Some((_, c)) = self.inner.peek().copied() &&
                    c != '{' &&
                    !is_separator(c) &&
                    c != '('
                {
                    self.emit_error(format!("expected ` `, `{{` or `,` after `\"`, found `{c}`"));
                    return None;
                }
                return Some(LangStringToken::LangToken(&self.data[indices.start..indices.end]));
            } else if c == '{' {
                self.is_in_attribute_block = true;
                return self.next();
            } else if is_separator(c) {
                if pos != start {
                    return Some(LangStringToken::LangToken(&self.data[start..pos]));
                }
                return self.next();
            } else if c == '(' {
                if !self.skip_paren_block() {
                    return None;
                }
                if pos != start {
                    return Some(LangStringToken::LangToken(&self.data[start..pos]));
                }
                return self.next();
            } else if pos == start && is_leading_char(c) {
                continue;
            } else if pos != start && is_bareword_char(c) {
                continue;
            } else {
                self.emit_error(format!("unexpected character `{c}`"));
                return None;
            }
        }
        let token = &self.data[start..];
        if token.is_empty() { None } else { Some(LangStringToken::LangToken(&self.data[start..])) }
    }
}

impl<'a, 'tcx> Iterator for TagIterator<'a, 'tcx> {
    type Item = LangStringToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(start) = self.skip_separators() else {
            if self.is_in_attribute_block {
                self.emit_error("unclosed attribute block (`{}`): missing `}` at the end");
            }
            return None;
        };
        if self.is_in_attribute_block {
            self.parse_in_attribute_block()
        } else {
            self.parse_outside_attribute_block(start)
        }
    }
}

fn tokens(string: &str) -> impl Iterator<Item = LangStringToken<'_>> {
    // Pandoc, which Rust once used for generating documentation,
    // expects lang strings to be surrounded by `{}` and for each token
    // to be proceeded by a `.`. Since some of these lang strings are still
    // loose in the wild, we strip a pair of surrounding `{}` from the lang
    // string and a leading `.` from each token.

    let string = string.trim();

    let first = string.chars().next();
    let last = string.chars().last();

    let string =
        if first == Some('{') && last == Some('}') { &string[1..string.len() - 1] } else { string };

    string
        .split(|c| c == ',' || c == ' ' || c == '\t')
        .map(str::trim)
        .map(|token| token.strip_prefix('.').unwrap_or(token))
        .filter(|token| !token.is_empty())
        .map(|token| LangStringToken::LangToken(token))
}

impl Default for LangString {
    fn default() -> Self {
        Self {
            original: String::new(),
            should_panic: false,
            no_run: false,
            ignore: Ignore::None,
            rust: true,
            test_harness: false,
            compile_fail: false,
            error_codes: Vec::new(),
            edition: None,
            added_classes: Vec::new(),
            unknown: Vec::new(),
        }
    }
}

impl LangString {
    fn parse_without_check(
        string: &str,
        allow_error_code_check: ErrorCodes,
        enable_per_target_ignores: bool,
        custom_code_classes_in_docs: bool,
    ) -> Self {
        Self::parse(
            string,
            allow_error_code_check,
            enable_per_target_ignores,
            None,
            custom_code_classes_in_docs,
        )
    }

    fn parse(
        string: &str,
        allow_error_code_check: ErrorCodes,
        enable_per_target_ignores: bool,
        extra: Option<&ExtraInfo<'_>>,
        custom_code_classes_in_docs: bool,
    ) -> Self {
        let allow_error_code_check = allow_error_code_check.as_bool();
        let mut seen_rust_tags = false;
        let mut seen_other_tags = false;
        let mut seen_custom_tag = false;
        let mut data = LangString::default();
        let mut ignores = vec![];

        data.original = string.to_owned();

        let mut call = |tokens: &mut dyn Iterator<Item = LangStringToken<'_>>| {
            for token in tokens {
                match token {
                    LangStringToken::LangToken("should_panic") => {
                        data.should_panic = true;
                        seen_rust_tags = !seen_other_tags;
                    }
                    LangStringToken::LangToken("no_run") => {
                        data.no_run = true;
                        seen_rust_tags = !seen_other_tags;
                    }
                    LangStringToken::LangToken("ignore") => {
                        data.ignore = Ignore::All;
                        seen_rust_tags = !seen_other_tags;
                    }
                    LangStringToken::LangToken(x) if x.starts_with("ignore-") => {
                        if enable_per_target_ignores {
                            ignores.push(x.trim_start_matches("ignore-").to_owned());
                            seen_rust_tags = !seen_other_tags;
                        }
                    }
                    LangStringToken::LangToken("rust") => {
                        data.rust = true;
                        seen_rust_tags = true;
                    }
                    LangStringToken::LangToken("custom") => {
                        if custom_code_classes_in_docs {
                            seen_custom_tag = true;
                        } else {
                            seen_other_tags = true;
                        }
                    }
                    LangStringToken::LangToken("test_harness") => {
                        data.test_harness = true;
                        seen_rust_tags = !seen_other_tags || seen_rust_tags;
                    }
                    LangStringToken::LangToken("compile_fail") => {
                        data.compile_fail = true;
                        seen_rust_tags = !seen_other_tags || seen_rust_tags;
                        data.no_run = true;
                    }
                    LangStringToken::LangToken(x) if x.starts_with("edition") => {
                        data.edition = x[7..].parse::<Edition>().ok();
                    }
                    LangStringToken::LangToken(x)
                        if allow_error_code_check && x.starts_with('E') && x.len() == 5 =>
                    {
                        if x[1..].parse::<u32>().is_ok() {
                            data.error_codes.push(x.to_owned());
                            seen_rust_tags = !seen_other_tags || seen_rust_tags;
                        } else {
                            seen_other_tags = true;
                        }
                    }
                    LangStringToken::LangToken(x) if extra.is_some() => {
                        let s = x.to_lowercase();
                        if let Some((flag, help)) = if s == "compile-fail"
                            || s == "compile_fail"
                            || s == "compilefail"
                        {
                            Some((
                                "compile_fail",
                                "the code block will either not be tested if not marked as a rust one \
                                 or won't fail if it compiles successfully",
                            ))
                        } else if s == "should-panic" || s == "should_panic" || s == "shouldpanic" {
                            Some((
                                "should_panic",
                                "the code block will either not be tested if not marked as a rust one \
                                 or won't fail if it doesn't panic when running",
                            ))
                        } else if s == "no-run" || s == "no_run" || s == "norun" {
                            Some((
                                "no_run",
                                "the code block will either not be tested if not marked as a rust one \
                                 or will be run (which you might not want)",
                            ))
                        } else if s == "test-harness" || s == "test_harness" || s == "testharness" {
                            Some((
                                "test_harness",
                                "the code block will either not be tested if not marked as a rust one \
                                 or the code will be wrapped inside a main function",
                            ))
                        } else {
                            None
                        } {
                            if let Some(extra) = extra {
                                extra.error_invalid_codeblock_attr_with_help(
                                    format!("unknown attribute `{x}`. Did you mean `{flag}`?"),
                                    help,
                                );
                            }
                        }
                        seen_other_tags = true;
                        data.unknown.push(x.to_owned());
                    }
                    LangStringToken::LangToken(x) => {
                        seen_other_tags = true;
                        data.unknown.push(x.to_owned());
                    }
                    LangStringToken::KeyValueAttribute(key, value) => {
                        if custom_code_classes_in_docs {
                            if key == "class" {
                                data.added_classes.push(value.to_owned());
                            } else if let Some(extra) = extra {
                                extra.error_invalid_codeblock_attr(format!(
                                    "unsupported attribute `{key}`"
                                ));
                            }
                        } else {
                            seen_other_tags = true;
                        }
                    }
                    LangStringToken::ClassAttribute(class) => {
                        data.added_classes.push(class.to_owned());
                    }
                }
            }
        };

        if custom_code_classes_in_docs {
            call(&mut TagIterator::new(string, extra).into_iter())
        } else {
            call(&mut tokens(string))
        }

        // ignore-foo overrides ignore
        if !ignores.is_empty() {
            data.ignore = Ignore::Some(ignores);
        }

        data.rust &= !seen_custom_tag && (!seen_other_tags || seen_rust_tags);

        data
    }
}

impl Markdown<'_> {
    pub fn into_string(self) -> String {
        let Markdown {
            content: md,
            links,
            ids,
            error_codes: codes,
            edition,
            playground,
            heading_offset,
            custom_code_classes_in_docs,
        } = self;

        // This is actually common enough to special-case
        if md.is_empty() {
            return String::new();
        }
        let mut replacer = |broken_link: BrokenLink<'_>| {
            links
                .iter()
                .find(|link| &*link.original_text == &*broken_link.reference)
                .map(|link| (link.href.as_str().into(), link.tooltip.as_str().into()))
        };

        let p = Parser::new_with_broken_link_callback(md, main_body_opts(), Some(&mut replacer));
        let p = p.into_offset_iter();

        let mut s = String::with_capacity(md.len() * 3 / 2);

        let p = HeadingLinks::new(p, None, ids, heading_offset);
        let p = Footnotes::new(p);
        let p = LinkReplacer::new(p.map(|(ev, _)| ev), links);
        let p = TableWrapper::new(p);
        let p = CodeBlocks::new(p, codes, edition, playground, custom_code_classes_in_docs);
        html::push_html(&mut s, p);

        s
    }
}

impl MarkdownWithToc<'_> {
    pub(crate) fn into_string(self) -> String {
        let MarkdownWithToc {
            content: md,
            ids,
            error_codes: codes,
            edition,
            playground,
            custom_code_classes_in_docs,
        } = self;

        let p = Parser::new_ext(md, main_body_opts()).into_offset_iter();

        let mut s = String::with_capacity(md.len() * 3 / 2);

        let mut toc = TocBuilder::new();

        {
            let p = HeadingLinks::new(p, Some(&mut toc), ids, HeadingOffset::H1);
            let p = Footnotes::new(p);
            let p = TableWrapper::new(p.map(|(ev, _)| ev));
            let p = CodeBlocks::new(p, codes, edition, playground, custom_code_classes_in_docs);
            html::push_html(&mut s, p);
        }

        format!("<nav id=\"TOC\">{toc}</nav>{s}", toc = toc.into_toc().print())
    }
}

impl MarkdownItemInfo<'_> {
    pub(crate) fn into_string(self) -> String {
        let MarkdownItemInfo(md, ids) = self;

        // This is actually common enough to special-case
        if md.is_empty() {
            return String::new();
        }
        let p = Parser::new_ext(md, main_body_opts()).into_offset_iter();

        // Treat inline HTML as plain text.
        let p = p.map(|event| match event.0 {
            Event::Html(text) => (Event::Text(text), event.1),
            _ => event,
        });

        let mut s = String::with_capacity(md.len() * 3 / 2);

        let p = HeadingLinks::new(p, None, ids, HeadingOffset::H1);
        let p = Footnotes::new(p);
        let p = TableWrapper::new(p.map(|(ev, _)| ev));
        let p = p.filter(|event| {
            !matches!(event, Event::Start(Tag::Paragraph) | Event::End(Tag::Paragraph))
        });
        html::push_html(&mut s, p);

        s
    }
}

impl MarkdownSummaryLine<'_> {
    pub(crate) fn into_string_with_has_more_content(self) -> (String, bool) {
        let MarkdownSummaryLine(md, links) = self;
        // This is actually common enough to special-case
        if md.is_empty() {
            return (String::new(), false);
        }

        let mut replacer = |broken_link: BrokenLink<'_>| {
            links
                .iter()
                .find(|link| &*link.original_text == &*broken_link.reference)
                .map(|link| (link.href.as_str().into(), link.tooltip.as_str().into()))
        };

        let p = Parser::new_with_broken_link_callback(md, summary_opts(), Some(&mut replacer))
            .peekable();
        let mut summary = SummaryLine::new(p);

        let mut s = String::new();

        let without_paragraphs = LinkReplacer::new(&mut summary, links).filter(|event| {
            !matches!(event, Event::Start(Tag::Paragraph) | Event::End(Tag::Paragraph))
        });

        html::push_html(&mut s, without_paragraphs);

        let has_more_content =
            matches!(summary.inner.peek(), Some(Event::Start(_))) || summary.skipped_tags > 0;

        (s, has_more_content)
    }

    pub(crate) fn into_string(self) -> String {
        self.into_string_with_has_more_content().0
    }
}

/// Renders a subset of Markdown in the first paragraph of the provided Markdown.
///
/// - *Italics*, **bold**, and `inline code` styles **are** rendered.
/// - Headings and links are stripped (though the text *is* rendered).
/// - HTML, code blocks, and everything else are ignored.
///
/// Returns a tuple of the rendered HTML string and whether the output was shortened
/// due to the provided `length_limit`.
fn markdown_summary_with_limit(
    md: &str,
    link_names: &[RenderedLink],
    length_limit: usize,
) -> (String, bool) {
    if md.is_empty() {
        return (String::new(), false);
    }

    let mut replacer = |broken_link: BrokenLink<'_>| {
        link_names
            .iter()
            .find(|link| &*link.original_text == &*broken_link.reference)
            .map(|link| (link.href.as_str().into(), link.tooltip.as_str().into()))
    };

    let p = Parser::new_with_broken_link_callback(md, summary_opts(), Some(&mut replacer));
    let mut p = LinkReplacer::new(p, link_names);

    let mut buf = HtmlWithLimit::new(length_limit);
    let mut stopped_early = false;
    p.try_for_each(|event| {
        match &event {
            Event::Text(text) => {
                let r =
                    text.split_inclusive(char::is_whitespace).try_for_each(|word| buf.push(word));
                if r.is_break() {
                    stopped_early = true;
                }
                return r;
            }
            Event::Code(code) => {
                buf.open_tag("code");
                let r = buf.push(code);
                if r.is_break() {
                    stopped_early = true;
                } else {
                    buf.close_tag();
                }
                return r;
            }
            Event::Start(tag) => match tag {
                Tag::Emphasis => buf.open_tag("em"),
                Tag::Strong => buf.open_tag("strong"),
                Tag::CodeBlock(..) => return ControlFlow::Break(()),
                _ => {}
            },
            Event::End(tag) => match tag {
                Tag::Emphasis | Tag::Strong => buf.close_tag(),
                Tag::Paragraph | Tag::Heading(..) => return ControlFlow::Break(()),
                _ => {}
            },
            Event::HardBreak | Event::SoftBreak => buf.push(" ")?,
            _ => {}
        };
        ControlFlow::Continue(())
    });

    (buf.finish(), stopped_early)
}

/// Renders a shortened first paragraph of the given Markdown as a subset of Markdown,
/// making it suitable for contexts like the search index.
///
/// Will shorten to 59 or 60 characters, including an ellipsis (…) if it was shortened.
///
/// See [`markdown_summary_with_limit`] for details about what is rendered and what is not.
pub(crate) fn short_markdown_summary(markdown: &str, link_names: &[RenderedLink]) -> String {
    let (mut s, was_shortened) = markdown_summary_with_limit(markdown, link_names, 59);

    if was_shortened {
        s.push('…');
    }

    s
}

/// Renders the first paragraph of the provided markdown as plain text.
/// Useful for alt-text.
///
/// - Headings, links, and formatting are stripped.
/// - Inline code is rendered as-is, surrounded by backticks.
/// - HTML and code blocks are ignored.
pub(crate) fn plain_text_summary(md: &str, link_names: &[RenderedLink]) -> String {
    if md.is_empty() {
        return String::new();
    }

    let mut s = String::with_capacity(md.len() * 3 / 2);

    let mut replacer = |broken_link: BrokenLink<'_>| {
        link_names
            .iter()
            .find(|link| &*link.original_text == &*broken_link.reference)
            .map(|link| (link.href.as_str().into(), link.tooltip.as_str().into()))
    };

    let p = Parser::new_with_broken_link_callback(md, summary_opts(), Some(&mut replacer));

    for event in p {
        match &event {
            Event::Text(text) => s.push_str(text),
            Event::Code(code) => {
                s.push('`');
                s.push_str(code);
                s.push('`');
            }
            Event::HardBreak | Event::SoftBreak => s.push(' '),
            Event::Start(Tag::CodeBlock(..)) => break,
            Event::End(Tag::Paragraph) => break,
            Event::End(Tag::Heading(..)) => break,
            _ => (),
        }
    }

    s
}

#[derive(Debug)]
pub(crate) struct MarkdownLink {
    pub kind: LinkType,
    pub link: String,
    pub display_text: Option<String>,
    pub range: MarkdownLinkRange,
}

#[derive(Clone, Debug)]
pub(crate) enum MarkdownLinkRange {
    /// Normally, markdown link warnings point only at the destination.
    Destination(Range<usize>),
    /// In some cases, it's not possible to point at the destination.
    /// Usually, this happens because backslashes `\\` are used.
    /// When that happens, point at the whole link, and don't provide structured suggestions.
    WholeLink(Range<usize>),
}

impl MarkdownLinkRange {
    /// Extracts the inner range.
    pub fn inner_range(&self) -> &Range<usize> {
        match self {
            MarkdownLinkRange::Destination(range) => range,
            MarkdownLinkRange::WholeLink(range) => range,
        }
    }
}

pub(crate) fn markdown_links<'md, R>(
    md: &'md str,
    preprocess_link: impl Fn(MarkdownLink) -> Option<R>,
) -> Vec<R> {
    if md.is_empty() {
        return vec![];
    }

    // FIXME: remove this function once pulldown_cmark can provide spans for link definitions.
    let locate = |s: &str, fallback: Range<usize>| unsafe {
        let s_start = s.as_ptr();
        let s_end = s_start.add(s.len());
        let md_start = md.as_ptr();
        let md_end = md_start.add(md.len());
        if md_start <= s_start && s_end <= md_end {
            let start = s_start.offset_from(md_start) as usize;
            let end = s_end.offset_from(md_start) as usize;
            MarkdownLinkRange::Destination(start..end)
        } else {
            MarkdownLinkRange::WholeLink(fallback)
        }
    };

    let span_for_link = |link: &CowStr<'_>, span: Range<usize>| {
        // For diagnostics, we want to underline the link's definition but `span` will point at
        // where the link is used. This is a problem for reference-style links, where the definition
        // is separate from the usage.

        match link {
            // `Borrowed` variant means the string (the link's destination) may come directly from
            // the markdown text and we can locate the original link destination.
            // NOTE: LinkReplacer also provides `Borrowed` but possibly from other sources,
            // so `locate()` can fall back to use `span`.
            CowStr::Borrowed(s) => locate(s, span),

            // For anything else, we can only use the provided range.
            CowStr::Boxed(_) | CowStr::Inlined(_) => MarkdownLinkRange::WholeLink(span),
        }
    };

    let span_for_offset_backward = |span: Range<usize>, open: u8, close: u8| {
        let mut open_brace = !0;
        let mut close_brace = !0;
        for (i, b) in md.as_bytes()[span.clone()].iter().copied().enumerate().rev() {
            let i = i + span.start;
            if b == close {
                close_brace = i;
                break;
            }
        }
        if close_brace < span.start || close_brace >= span.end {
            return MarkdownLinkRange::WholeLink(span);
        }
        let mut nesting = 1;
        for (i, b) in md.as_bytes()[span.start..close_brace].iter().copied().enumerate().rev() {
            let i = i + span.start;
            if b == close {
                nesting += 1;
            }
            if b == open {
                nesting -= 1;
            }
            if nesting == 0 {
                open_brace = i;
                break;
            }
        }
        assert!(open_brace != close_brace);
        if open_brace < span.start || open_brace >= span.end {
            return MarkdownLinkRange::WholeLink(span);
        }
        // do not actually include braces in the span
        let range = (open_brace + 1)..close_brace;
        MarkdownLinkRange::Destination(range.clone())
    };

    let span_for_offset_forward = |span: Range<usize>, open: u8, close: u8| {
        let mut open_brace = !0;
        let mut close_brace = !0;
        for (i, b) in md.as_bytes()[span.clone()].iter().copied().enumerate() {
            let i = i + span.start;
            if b == open {
                open_brace = i;
                break;
            }
        }
        if open_brace < span.start || open_brace >= span.end {
            return MarkdownLinkRange::WholeLink(span);
        }
        let mut nesting = 0;
        for (i, b) in md.as_bytes()[open_brace..span.end].iter().copied().enumerate() {
            let i = i + open_brace;
            if b == close {
                nesting -= 1;
            }
            if b == open {
                nesting += 1;
            }
            if nesting == 0 {
                close_brace = i;
                break;
            }
        }
        assert!(open_brace != close_brace);
        if open_brace < span.start || open_brace >= span.end {
            return MarkdownLinkRange::WholeLink(span);
        }
        // do not actually include braces in the span
        let range = (open_brace + 1)..close_brace;
        MarkdownLinkRange::Destination(range.clone())
    };

    let mut broken_link_callback = |link: BrokenLink<'md>| Some((link.reference, "".into()));
    let mut event_iter = Parser::new_with_broken_link_callback(
        md,
        main_body_opts(),
        Some(&mut broken_link_callback),
    )
    .into_offset_iter();
    let mut links = Vec::new();

    while let Some((event, span)) = event_iter.next() {
        match event {
            Event::Start(Tag::Link(link_type, dest, _)) if may_be_doc_link(link_type) => {
                let range = match link_type {
                    // Link is pulled from the link itself.
                    LinkType::ReferenceUnknown | LinkType::ShortcutUnknown => {
                        span_for_offset_backward(span, b'[', b']')
                    }
                    LinkType::CollapsedUnknown => span_for_offset_forward(span, b'[', b']'),
                    LinkType::Inline => span_for_offset_backward(span, b'(', b')'),
                    // Link is pulled from elsewhere in the document.
                    LinkType::Reference | LinkType::Collapsed | LinkType::Shortcut => {
                        span_for_link(&dest, span)
                    }
                    LinkType::Autolink | LinkType::Email => unreachable!(),
                };

                let display_text = if matches!(
                    link_type,
                    LinkType::Inline
                        | LinkType::ReferenceUnknown
                        | LinkType::Reference
                        | LinkType::Shortcut
                        | LinkType::ShortcutUnknown
                ) {
                    collect_link_data(&mut event_iter)
                } else {
                    None
                };

                if let Some(link) = preprocess_link(MarkdownLink {
                    kind: link_type,
                    link: dest.into_string(),
                    display_text,
                    range,
                }) {
                    links.push(link);
                }
            }
            _ => {}
        }
    }

    links
}

/// Collects additional data of link.
fn collect_link_data<'input, 'callback>(
    event_iter: &mut OffsetIter<'input, 'callback>,
) -> Option<String> {
    let mut display_text: Option<String> = None;
    let mut append_text = |text: CowStr<'_>| {
        if let Some(display_text) = &mut display_text {
            display_text.push_str(&text);
        } else {
            display_text = Some(text.to_string());
        }
    };

    while let Some((event, _span)) = event_iter.next() {
        match event {
            Event::Text(text) => {
                append_text(text);
            }
            Event::Code(code) => {
                append_text(code);
            }
            Event::End(_) => {
                break;
            }
            _ => {}
        }
    }

    display_text
}

#[derive(Debug)]
pub(crate) struct RustCodeBlock {
    /// The range in the markdown that the code block occupies. Note that this includes the fences
    /// for fenced code blocks.
    pub(crate) range: Range<usize>,
    /// The range in the markdown that the code within the code block occupies.
    pub(crate) code: Range<usize>,
    pub(crate) is_fenced: bool,
    pub(crate) lang_string: LangString,
}

/// Returns a range of bytes for each code block in the markdown that is tagged as `rust` or
/// untagged (and assumed to be rust).
pub(crate) fn rust_code_blocks(
    md: &str,
    extra_info: &ExtraInfo<'_>,
    custom_code_classes_in_docs: bool,
) -> Vec<RustCodeBlock> {
    let mut code_blocks = vec![];

    if md.is_empty() {
        return code_blocks;
    }

    let mut p = Parser::new_ext(md, main_body_opts()).into_offset_iter();

    while let Some((event, offset)) = p.next() {
        if let Event::Start(Tag::CodeBlock(syntax)) = event {
            let (lang_string, code_start, code_end, range, is_fenced) = match syntax {
                CodeBlockKind::Fenced(syntax) => {
                    let syntax = syntax.as_ref();
                    let lang_string = if syntax.is_empty() {
                        Default::default()
                    } else {
                        LangString::parse(
                            &*syntax,
                            ErrorCodes::Yes,
                            false,
                            Some(extra_info),
                            custom_code_classes_in_docs,
                        )
                    };
                    if !lang_string.rust {
                        continue;
                    }
                    let (code_start, mut code_end) = match p.next() {
                        Some((Event::Text(_), offset)) => (offset.start, offset.end),
                        Some((_, sub_offset)) => {
                            let code = Range { start: sub_offset.start, end: sub_offset.start };
                            code_blocks.push(RustCodeBlock {
                                is_fenced: true,
                                range: offset,
                                code,
                                lang_string,
                            });
                            continue;
                        }
                        None => {
                            let code = Range { start: offset.end, end: offset.end };
                            code_blocks.push(RustCodeBlock {
                                is_fenced: true,
                                range: offset,
                                code,
                                lang_string,
                            });
                            continue;
                        }
                    };
                    while let Some((Event::Text(_), offset)) = p.next() {
                        code_end = offset.end;
                    }
                    (lang_string, code_start, code_end, offset, true)
                }
                CodeBlockKind::Indented => {
                    // The ending of the offset goes too far sometime so we reduce it by one in
                    // these cases.
                    if offset.end > offset.start && md.get(offset.end..=offset.end) == Some("\n") {
                        (
                            LangString::default(),
                            offset.start,
                            offset.end,
                            Range { start: offset.start, end: offset.end - 1 },
                            false,
                        )
                    } else {
                        (LangString::default(), offset.start, offset.end, offset, false)
                    }
                }
            };

            code_blocks.push(RustCodeBlock {
                is_fenced,
                range,
                code: Range { start: code_start, end: code_end },
                lang_string,
            });
        }
    }

    code_blocks
}

#[derive(Clone, Default, Debug)]
pub struct IdMap {
    map: FxHashMap<Cow<'static, str>, usize>,
}

// The map is pre-initialized and cloned each time to avoid reinitializing it repeatedly.
static DEFAULT_ID_MAP: Lazy<FxHashMap<Cow<'static, str>, usize>> = Lazy::new(|| init_id_map());

fn init_id_map() -> FxHashMap<Cow<'static, str>, usize> {
    let mut map = FxHashMap::default();
    // This is the list of IDs used in JavaScript.
    map.insert("help".into(), 1);
    map.insert("settings".into(), 1);
    map.insert("not-displayed".into(), 1);
    map.insert("alternative-display".into(), 1);
    map.insert("search".into(), 1);
    map.insert("crate-search".into(), 1);
    map.insert("crate-search-div".into(), 1);
    // This is the list of IDs used in HTML generated in Rust (including the ones
    // used in tera template files).
    map.insert("themeStyle".into(), 1);
    map.insert("settings-menu".into(), 1);
    map.insert("help-button".into(), 1);
    map.insert("main-content".into(), 1);
    map.insert("toggle-all-docs".into(), 1);
    map.insert("all-types".into(), 1);
    map.insert("default-settings".into(), 1);
    map.insert("sidebar-vars".into(), 1);
    map.insert("copy-path".into(), 1);
    map.insert("TOC".into(), 1);
    // This is the list of IDs used by rustdoc sections (but still generated by
    // rustdoc).
    map.insert("fields".into(), 1);
    map.insert("variants".into(), 1);
    map.insert("implementors-list".into(), 1);
    map.insert("synthetic-implementors-list".into(), 1);
    map.insert("foreign-impls".into(), 1);
    map.insert("implementations".into(), 1);
    map.insert("trait-implementations".into(), 1);
    map.insert("synthetic-implementations".into(), 1);
    map.insert("blanket-implementations".into(), 1);
    map.insert("required-associated-types".into(), 1);
    map.insert("provided-associated-types".into(), 1);
    map.insert("provided-associated-consts".into(), 1);
    map.insert("required-associated-consts".into(), 1);
    map.insert("required-methods".into(), 1);
    map.insert("provided-methods".into(), 1);
    map.insert("implementors".into(), 1);
    map.insert("synthetic-implementors".into(), 1);
    map.insert("implementations-list".into(), 1);
    map.insert("trait-implementations-list".into(), 1);
    map.insert("synthetic-implementations-list".into(), 1);
    map.insert("blanket-implementations-list".into(), 1);
    map.insert("deref-methods".into(), 1);
    map.insert("layout".into(), 1);
    map.insert("aliased-type".into(), 1);
    map
}

impl IdMap {
    pub fn new() -> Self {
        IdMap { map: DEFAULT_ID_MAP.clone() }
    }

    pub(crate) fn derive<S: AsRef<str> + ToString>(&mut self, candidate: S) -> String {
        let id = match self.map.get_mut(candidate.as_ref()) {
            None => candidate.to_string(),
            Some(a) => {
                let id = format!("{}-{}", candidate.as_ref(), *a);
                *a += 1;
                id
            }
        };

        self.map.insert(id.clone().into(), 1);
        id
    }
}
