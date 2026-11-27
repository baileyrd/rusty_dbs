//! HTML-to-markdown walk over a small tag set. url2obs kept the article
//! as HTML; this is new, hand-written to avoid a converter dependency.
//!
//! Supported: headings, paragraphs, links, ordered and unordered lists
//! (nested), `pre`/`code`, blockquotes, emphasis, images, simple tables
//! and horizontal rules. Anything else is a transparent container: its
//! children render, the tag does not.

use ego_tree::NodeId;
use scraper::{ElementRef, Node};
use url::Url;

use crate::dom::Dom;
use crate::patterns::BLOCK_TAGS;

/// Cells holding any of these make a table a layout table, rendered as
/// plain blocks instead of a pipe table (common on old pages).
const LAYOUT_CELL_TAGS: &[&str] = &[
    "p",
    "div",
    "table",
    "ul",
    "ol",
    "pre",
    "blockquote",
    "h1",
    "h2",
    "h3",
];

const LIST_INDENT: &str = "    ";

/// Heading permalink anchors ("#", "¶"): navigation, not content.
const PERMALINK_TEXT: &[&str] = &["#", "¶", "§", "🔗"];

/// Renders the article roots to markdown: blocks separated by a blank
/// line, ending in one newline. Empty when nothing renders.
pub(crate) fn render(dom: &Dom, roots: &[NodeId], base: &Url) -> String {
    let renderer = Renderer { dom, base };
    let mut blocks = Vec::new();
    for root in roots.iter().filter_map(|id| dom.element(*id)) {
        renderer.block(root, &mut blocks);
    }
    let body = blocks.join("\n\n");
    let body = body.trim();
    if body.is_empty() {
        return String::new();
    }
    format!("{body}\n")
}

struct Renderer<'a> {
    dom: &'a Dom,
    base: &'a Url,
}

impl Renderer<'_> {
    fn block(&self, el: ElementRef<'_>, out: &mut Vec<String>) {
        let name = el.value().name();
        let rendered = match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.heading(el, name),
            "p" => self.paragraph(el),
            "pre" => Some(self.code_block(el)),
            "ul" | "ol" => Some(self.list(el, 0)).filter(|l| !l.is_empty()),
            "blockquote" => self.blockquote(el),
            "table" if !self.is_layout_table(el) => self.table(el),
            "hr" => Some("---".to_string()),
            _ => {
                self.children_as_blocks(el, out);
                None
            }
        };
        out.extend(rendered);
    }

    /// Children as blocks; runs of text and inline elements between block
    /// children become paragraphs.
    fn children_as_blocks(&self, el: ElementRef<'_>, out: &mut Vec<String>) {
        let mut run = String::new();
        for child in el.children().filter(|n| self.dom.is_live(*n)) {
            if let Some(child_el) = ElementRef::wrap(child) {
                if BLOCK_TAGS.contains(&child_el.value().name()) {
                    flush(&mut run, out);
                    self.block(child_el, out);
                    continue;
                }
                run.push_str(&self.inline(child_el));
                continue;
            }
            if let Node::Text(text) = child.value() {
                run.push_str(&collapse_keeping_edges(text));
            }
        }
        flush(&mut run, out);
    }

    fn heading(&self, el: ElementRef<'_>, name: &str) -> Option<String> {
        let level: usize = name[1..].parse().unwrap_or(1);
        let text = one_line(&self.inline_children(el));
        (!text.is_empty()).then(|| format!("{} {text}", "#".repeat(level)))
    }

    fn paragraph(&self, el: ElementRef<'_>) -> Option<String> {
        let text = tidy(&self.inline_children(el));
        (!text.is_empty()).then_some(text)
    }

    fn code_block(&self, el: ElementRef<'_>) -> String {
        let code = self.raw_text(el);
        let code = code.trim_matches('\n');
        let fence = if code.contains("```") { "````" } else { "```" };
        format!("{fence}{}\n{code}\n{fence}", self.code_language(el))
    }

    /// `language-x` or `lang-x` on the `pre` or its first `code`.
    fn code_language(&self, pre: ElementRef<'_>) -> String {
        let code = pre
            .children()
            .filter_map(ElementRef::wrap)
            .find(|c| c.value().name() == "code");
        std::iter::once(pre)
            .chain(code)
            .flat_map(|el| el.value().classes().collect::<Vec<_>>())
            .find_map(|c| {
                c.strip_prefix("language-")
                    .or_else(|| c.strip_prefix("lang-"))
            })
            .unwrap_or("")
            .to_string()
    }

    fn list(&self, el: ElementRef<'_>, depth: usize) -> String {
        let ordered = el.value().name() == "ol";
        let items = self
            .dom
            .live_children(el)
            .filter(|c| c.value().name() == "li");
        let mut lines = Vec::new();
        for (index, li) in items.enumerate() {
            let marker = if ordered {
                format!("{}.", index + 1)
            } else {
                "-".to_string()
            };
            let (text, nested) = self.list_item(li, depth);
            lines.push(
                format!("{}{marker} {text}", LIST_INDENT.repeat(depth))
                    .trim_end()
                    .to_string(),
            );
            lines.extend(nested);
        }
        lines.join("\n")
    }

    /// The item's own text (nested blocks flattened inline) and its
    /// nested lists, already rendered one level deeper.
    fn list_item(&self, li: ElementRef<'_>, depth: usize) -> (String, Vec<String>) {
        let mut text = String::new();
        let mut nested = Vec::new();
        for child in li.children().filter(|n| self.dom.is_live(*n)) {
            match ElementRef::wrap(child) {
                Some(c) if matches!(c.value().name(), "ul" | "ol") => {
                    nested.push(self.list(c, depth + 1));
                }
                Some(c) if BLOCK_TAGS.contains(&c.value().name()) => {
                    text.push(' ');
                    text.push_str(&self.inline(c));
                    text.push(' ');
                }
                Some(c) => text.push_str(&self.inline(c)),
                None => {
                    if let Node::Text(t) = child.value() {
                        text.push_str(&collapse_keeping_edges(t));
                    }
                }
            }
        }
        let text = one_line(&text);
        (text, nested.into_iter().filter(|n| !n.is_empty()).collect())
    }

    fn blockquote(&self, el: ElementRef<'_>) -> Option<String> {
        let mut inner = Vec::new();
        self.children_as_blocks(el, &mut inner);
        let text = inner.join("\n\n");
        if text.trim().is_empty() {
            return None;
        }
        let quoted: Vec<String> = text
            .lines()
            .map(|line| {
                if line.is_empty() {
                    ">".to_string()
                } else {
                    format!("> {line}")
                }
            })
            .collect();
        Some(quoted.join("\n"))
    }

    fn is_layout_table(&self, table: ElementRef<'_>) -> bool {
        self.dom
            .live_elements(table)
            .filter(|el| matches!(el.value().name(), "td" | "th"))
            .any(|cell| {
                self.dom
                    .live_elements(cell)
                    .skip(1)
                    .any(|inner| LAYOUT_CELL_TAGS.contains(&inner.value().name()))
            })
    }

    fn table(&self, table: ElementRef<'_>) -> Option<String> {
        let rows: Vec<Vec<String>> = self
            .dom
            .live_elements(table)
            .filter(|el| el.value().name() == "tr")
            .map(|tr| self.table_row(tr))
            .filter(|row| !row.is_empty())
            .collect();
        let width = rows.iter().map(Vec::len).max()?;
        let line = |cells: &[String]| {
            let padded: Vec<&str> = (0..width)
                .map(|i| cells.get(i).map_or("", String::as_str))
                .collect();
            format!("| {} |", padded.join(" | "))
        };
        let mut lines = vec![line(&rows[0]), format!("|{}", " --- |".repeat(width))];
        lines.extend(rows[1..].iter().map(|r| line(r)));
        Some(lines.join("\n"))
    }

    fn table_row(&self, tr: ElementRef<'_>) -> Vec<String> {
        self.dom
            .live_children(tr)
            .filter(|c| matches!(c.value().name(), "td" | "th"))
            .map(|cell| one_line(&self.inline_children(cell)).replace('|', "\\|"))
            .collect()
    }

    /// An inline element, keeping the whitespace at its edges outside any
    /// markers so words do not run together.
    fn inline(&self, el: ElementRef<'_>) -> String {
        match el.value().name() {
            "a" => self.link(el),
            "em" | "i" => wrap(&self.inline_children(el), "*"),
            "strong" | "b" => wrap(&self.inline_children(el), "**"),
            "code" => inline_code(&self.raw_text(el)),
            "img" => self.image(el),
            "br" => "\n".to_string(),
            "pre" => format!("\n{}\n", self.code_block(el)),
            _ => self.inline_children(el),
        }
    }

    fn inline_children(&self, el: ElementRef<'_>) -> String {
        let mut out = String::new();
        for child in el.children().filter(|n| self.dom.is_live(*n)) {
            if let Some(child_el) = ElementRef::wrap(child) {
                let rendered = self.inline(child_el);
                let spaced = BLOCK_TAGS.contains(&child_el.value().name());
                if spaced {
                    out.push(' ');
                }
                out.push_str(&rendered);
                if spaced {
                    out.push(' ');
                }
                continue;
            }
            if let Node::Text(text) = child.value() {
                out.push_str(&collapse_keeping_edges(text));
            }
        }
        out
    }

    fn link(&self, a: ElementRef<'_>) -> String {
        let inner = self.inline_children(a);
        let text = one_line(&inner);
        let href = a.value().attr("href").and_then(|h| self.resolve(h));
        if PERMALINK_TEXT.contains(&text.as_str()) {
            return String::new();
        }
        match (text.is_empty(), href) {
            (true, _) => String::new(),
            (false, None) => inner,
            (false, Some(href)) => with_edges(&inner, &format!("[{text}]({href})")),
        }
    }

    /// url2obs prefers lazy-load sources: `data-src`, then `src`, then
    /// `data-original`. Images with no usable source are dropped.
    fn image(&self, img: ElementRef<'_>) -> String {
        let attrs = img.value();
        let src = ["data-src", "src", "data-original"]
            .iter()
            .filter_map(|name| attrs.attr(name))
            .find_map(|raw| self.resolve(raw));
        let Some(src) = src else {
            return String::new();
        };
        let alt = attrs
            .attr("alt")
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or("Image");
        format!("![{}]({src})", alt.replace(['[', ']'], ""))
    }

    /// An absolute http(s) URL, or `None` for empty, script, inline-data
    /// and unparseable references.
    fn resolve(&self, raw: &str) -> Option<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let url = self.base.join(raw).ok()?;
        matches!(url.scheme(), "http" | "https").then(|| url.to_string())
    }

    /// Text under `el` exactly as written (for code).
    fn raw_text(&self, el: ElementRef<'_>) -> String {
        el.descendants()
            .filter(|n| self.dom.is_live(*n))
            .filter_map(|n| n.value().as_text().map(|t| t.to_string()))
            .collect()
    }
}

fn flush(run: &mut String, out: &mut Vec<String>) {
    let text = tidy(run);
    if !text.is_empty() {
        out.push(text);
    }
    run.clear();
}

/// Collapses whitespace runs to one space, keeping one space at either
/// edge if the text had whitespace there.
fn collapse_keeping_edges(text: &str) -> String {
    let core = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if core.is_empty() {
        return if text.is_empty() {
            String::new()
        } else {
            " ".to_string()
        };
    }
    with_edges(text, &core)
}

/// `body` with a space before/after it if `original` had whitespace there.
fn with_edges(original: &str, body: &str) -> String {
    let lead = if original.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trail = if original.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    format!("{lead}{body}{trail}")
}

fn wrap(inner: &str, marker: &str) -> String {
    let core = tidy(inner);
    if core.is_empty() {
        return inner.to_string();
    }
    with_edges(inner, &format!("{marker}{core}{marker}"))
}

fn inline_code(code: &str) -> String {
    let code = code.trim();
    if code.is_empty() {
        return String::new();
    }
    if code.contains('`') {
        return format!("`` {code} ``");
    }
    format!("`{code}`")
}

/// Collapses spaces on every line and trims. Single line breaks (one
/// `<br>`) survive; a blank line (`<br><br>`) becomes one paragraph break.
fn tidy(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let after_blank = lines.last().is_none_or(|l| l.is_empty());
        if line.is_empty() && after_blank {
            continue;
        }
        lines.push(line);
    }
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// `text` on one line, whitespace collapsed (headings, cells, links).
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Selector;

    fn md(body: &str) -> String {
        let dom = Dom::parse_and_clean(&format!(
            "<html><body><div id=\"root\">{body}</div></body></html>"
        ));
        let sel = Selector::parse("#root").expect("selector");
        let root = dom.doc.select(&sel).next().expect("root").id();
        let base = Url::parse("https://example.com/blog/post").expect("base url");
        render(&dom, &[root], &base)
    }

    #[test]
    fn headings_paragraphs_and_inline_marks() {
        assert_eq!(
            md("<h2>Title  here</h2><p>Some <em>soft</em> and <strong>bold </strong>text, <code>x()</code>.</p>"),
            "## Title here\n\nSome *soft* and **bold** text, `x()`.\n"
        );
    }

    #[test]
    fn links_and_images_resolve_against_the_source() {
        assert_eq!(
            md(
                r##"<p>See <a href="../about">the page</a> <a href="javascript:void(0)">js</a><a href="#x">¶</a> <img data-src="/i.png" src="x.gif"><img alt="none"></p>"##
            ),
            "See [the page](https://example.com/about) js ![Image](https://example.com/i.png)\n"
        );
    }

    #[test]
    fn nested_lists() {
        assert_eq!(
            md("<ul><li>one<ul><li>inner</li></ul></li><li><p>two</p><em>x</em>.</li></ul><ol><li>a</li><li>b</li></ol>"),
            "- one\n    - inner\n- two *x*.\n\n1. a\n2. b\n"
        );
    }

    #[test]
    fn code_blocks_keep_whitespace_and_language() {
        assert_eq!(
            md("<pre><code class=\"language-rust\">fn main() {\n    run();\n}\n</code></pre>"),
            "```rust\nfn main() {\n    run();\n}\n```\n"
        );
    }

    #[test]
    fn blockquote_and_rule() {
        assert_eq!(
            md("<blockquote><p>a</p><p>b</p></blockquote><hr>"),
            "> a\n>\n> b\n\n---\n"
        );
    }

    #[test]
    fn simple_table_becomes_pipes_and_layout_table_becomes_blocks() {
        assert_eq!(
            md("<table><tr><th>k</th><th>v</th></tr><tr><td>a|b</td></tr></table>"),
            "| k | v |\n| --- | --- |\n| a\\|b |  |\n"
        );
        assert_eq!(
            md("<table><tr><td><p>left</p></td><td><p>right</p></td></tr></table>"),
            "left\n\nright\n"
        );
    }

    #[test]
    fn mixed_text_and_blocks_split_into_paragraphs() {
        assert_eq!(
            md("loose <b>text</b><p>para</p>tail"),
            "loose **text**\n\npara\n\ntail\n"
        );
    }

    #[test]
    fn double_breaks_split_paragraphs_and_single_breaks_stay() {
        assert_eq!(
            md("<font>one<br>two<br><br> three</font>"),
            "one\ntwo\n\nthree\n"
        );
    }

    #[test]
    fn nothing_renders_to_empty() {
        assert_eq!(md("<p>  </p><div></div>"), "");
    }
}
