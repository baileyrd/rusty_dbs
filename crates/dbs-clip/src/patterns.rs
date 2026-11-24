//! Class/id and tag-name patterns, ported from url2obs's `PatternManager`.
//!
//! url2obs compiles case-insensitive regexes that are plain alternations
//! of substrings, so a lowercase substring test is equivalent and keeps
//! `regex` out of the dependency list.

use scraper::ElementRef;

/// Class/id fragments that mark boilerplate (comments, navigation, ads).
const NEGATIVE: &[&str] = &[
    "comment",
    "meta",
    "footer",
    "footnote",
    "foot",
    "header",
    "menu",
    "sidebar",
    "navigation",
    "nav",
    "banner",
    "ad-",
    "combx",
    "disqus",
    "community",
    "share",
    "extra",
    "rss",
    "shoutbox",
    "sponsor",
    "hidden",
    "social",
    "widget",
    "related",
    "recommended",
    "popular",
];

/// Class/id fragments that mark article content.
const POSITIVE: &[&str] = &[
    "article", "body", "content", "entry", "main", "page", "post", "text", "blog", "story",
    "column", "shadow",
];

/// Class/id fragments that mark code; such elements are never removed.
const CODE: &[&str] = &[
    "code",
    "pre",
    "syntax",
    "highlight",
    "snippet",
    "gist",
    "codeblock",
];

/// Class fragments that mark instruction or prompt blocks (preserved whole).
const INSTRUCTION: &[&str] = &[
    "instruction",
    "prompt",
    "exercise",
    "tutorial",
    "task",
    "assignment",
];

/// Tags that never hold article content.
const UNLIKELY_TAGS: &[&str] = &[
    "aside", "footer", "nav", "menu", "style", "script", "head", "iframe", "noscript",
];

/// Tags always removed, preserved descendants or not.
pub(crate) const ALWAYS_REMOVED_TAGS: &[&str] = &["script", "style", "noscript"];

/// Tags whose content is code and is kept verbatim.
const PRESERVE_TAGS: &[&str] = &["pre", "code"];

/// Tags that start a new block when met inside running text.
pub(crate) const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// The element's class list and id joined by spaces, lowercased.
pub(crate) fn attr_string(el: ElementRef<'_>) -> String {
    let value = el.value();
    let classes: Vec<&str> = value.classes().collect();
    let id = value.id().unwrap_or("");
    format!("{} {}", classes.join(" "), id)
        .trim()
        .to_lowercase()
}

fn matches_any(attrs: &str, fragments: &[&str]) -> bool {
    !attrs.is_empty() && fragments.iter().any(|f| attrs.contains(f))
}

pub(crate) fn is_negative(attrs: &str) -> bool {
    matches_any(attrs, NEGATIVE)
}

pub(crate) fn is_positive(attrs: &str) -> bool {
    matches_any(attrs, POSITIVE)
}

/// The element's class list joined by spaces, lowercased.
fn class_string(el: ElementRef<'_>) -> String {
    let classes: Vec<String> = el.value().classes().map(str::to_lowercase).collect();
    classes.join(" ")
}

/// url2obs matches instruction patterns against the class list only.
pub(crate) fn is_instruction(el: ElementRef<'_>) -> bool {
    matches_any(&class_string(el), INSTRUCTION)
}

/// Code is recognised by tag or class only. url2obs also matched the id,
/// which made every GitHub gist comment (`id="gistcomment-…"`) count as
/// code, so comment threads could never be pruned.
pub(crate) fn is_code_element(el: ElementRef<'_>) -> bool {
    is_preserve_tag(el.value().name()) || matches_any(&class_string(el), CODE)
}

pub(crate) fn is_unlikely_tag(name: &str) -> bool {
    UNLIKELY_TAGS.contains(&name)
}

pub(crate) fn is_preserve_tag(name: &str) -> bool {
    PRESERVE_TAGS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_substrings_case_insensitively_on_lowercased_input() {
        assert!(is_negative("site-header"));
        assert!(is_negative("ad-slot"));
        assert!(!is_negative("article-body"));
        assert!(is_positive("entry-content"));
        assert!(!is_negative(""));
    }

    #[test]
    fn code_is_recognised_by_tag_or_class_but_not_id() {
        let doc = scraper::Html::parse_fragment(
            r#"<pre id="a"></pre><div class="highlight-source-rust"></div><div id="gistcomment-1"></div>"#,
        );
        let flags: Vec<bool> = doc
            .root_element()
            .children()
            .filter_map(ElementRef::wrap)
            .map(is_code_element)
            .collect();
        assert_eq!(flags, [true, true, false]);
    }

    #[test]
    fn unlikely_and_preserve_tags() {
        assert!(is_unlikely_tag("aside"));
        assert!(!is_unlikely_tag("div"));
        assert!(is_preserve_tag("pre"));
        assert!(!is_preserve_tag("p"));
    }
}
