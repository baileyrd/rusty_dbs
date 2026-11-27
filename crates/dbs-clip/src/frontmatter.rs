//! YAML frontmatter: url2obs's keys first, then the pipeline's.
//!
//! url2obs wrote values unquoted, so a title holding `: ` or `#` broke
//! the YAML. Every string here is double-quoted and escaped, and tags are
//! a YAML list (Obsidian's form) instead of url2obs's space-joined string.

use std::fmt::Write as _;

use crate::metadata::PageMeta;
use crate::SourceMeta;

pub(crate) fn render(
    title: &str,
    page: &PageMeta,
    meta: &SourceMeta,
    content_hash: &str,
) -> String {
    let author = if page.author.is_empty() {
        String::new()
    } else {
        format!("[[{}]]", page.author)
    };
    let mut out = String::from("---\n");
    scalar(&mut out, "category", &quote("[[Clippings]]"));
    scalar(&mut out, "author", &quote(&author));
    scalar(&mut out, "title", &quote(title));
    scalar(&mut out, "description", &quote(&page.description));
    scalar(&mut out, "source", &quote(meta.source.as_str()));
    scalar(&mut out, "clipped", &meta.clipped.to_string());
    scalar(
        &mut out,
        "published",
        &page.published.map(|d| d.to_string()).unwrap_or_default(),
    );
    list(&mut out, "tags", &merge_tags(&meta.tags, &page.keywords));
    scalar(
        &mut out,
        "raindrop_id",
        &meta
            .raindrop_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
    );
    scalar(
        &mut out,
        "saved",
        &meta.saved.map(|d| d.to_string()).unwrap_or_default(),
    );
    list(&mut out, "highlights", &meta.highlights);
    scalar(&mut out, "fetch_path", meta.fetch_path.as_str());
    scalar(&mut out, "content_hash", content_hash);
    out.push_str("---\n");
    out
}

/// Bookmark tags first, then page keywords; trimmed, first spelling of
/// each (case-insensitively) kept.
fn merge_tags(bookmark: &[String], keywords: &[String]) -> Vec<String> {
    let mut seen = Vec::<String>::new();
    let mut tags = Vec::new();
    for tag in bookmark.iter().chain(keywords).map(|t| t.trim()) {
        let key = tag.to_lowercase();
        if tag.is_empty() || seen.contains(&key) {
            continue;
        }
        seen.push(key);
        tags.push(tag.to_string());
    }
    tags
}

/// `key: value`, or a bare `key:` (YAML null) when the value is empty.
fn scalar(out: &mut String, key: &str, value: &str) {
    if value.is_empty() {
        let _ = writeln!(out, "{key}:");
        return;
    }
    let _ = writeln!(out, "{key}: {value}");
}

fn list(out: &mut String, key: &str, items: &[String]) {
    if items.is_empty() {
        let _ = writeln!(out, "{key}: []");
        return;
    }
    let _ = writeln!(out, "{key}:");
    for item in items {
        let _ = writeln!(out, "  - {}", quote(item));
    }
}

/// A YAML double-quoted scalar; empty input stays empty (null).
fn quote(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes_yaml_specials() {
        assert_eq!(quote("a: \"b\" # c\\d\n"), r#""a: \"b\" # c\\d\n""#);
        assert_eq!(quote("\u{7}"), r#""\u0007""#);
        assert_eq!(quote(""), "");
    }

    #[test]
    fn tags_merge_bookmark_first_without_case_duplicates() {
        let bookmark = vec!["Rust".to_string(), " ".to_string()];
        let keywords = vec!["rust".to_string(), "html".to_string()];
        assert_eq!(merge_tags(&bookmark, &keywords), ["Rust", "html"]);
    }
}
