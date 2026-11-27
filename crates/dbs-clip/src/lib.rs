//! Clip core for the knowledge pipeline (Mill Board #1306, parent #763):
//! HTML in, Obsidian-style markdown with YAML frontmatter out.
//!
//! A Rust port of url2obs's extractor (baileyrd/url2obs@9d47b33,
//! `src/content_extractor.py`): `MetadataExtractor` ([`metadata`]),
//! `HtmlCleaner` ([`dom`]), `ContentScorer` ([`score`]) and
//! `ContentFormatter` ([`article`]). url2obs kept the article as HTML; the
//! markdown walk ([`markdown`]) is new.
//!
//! [`clip`] is pure: no I/O, no clock (the clip date is an input), and
//! deterministic, so the same page and metadata always give the same
//! bytes and the same [`Clipped::content_hash`].
//!
//! # Differences from url2obs
//!
//! The port keeps url2obs's patterns, thresholds and order of passes.
//! Each change below fixes a wrong pick seen on a real page:
//!
//! * Blocks with their own text and no block children vote like `<p>`,
//!   and boilerplate class names cost as much as content ones earn
//!   (Mozilla Readability's rules; arXiv abstracts, gists, old pages).
//! * Code is recognised by tag or class, not id (gist comment ids).
//! * The final cleanup never removes the chosen article root, and the
//!   document root is never a candidate.
//! * Frontmatter strings are quoted YAML and tags are a list; heading
//!   permalinks (`#`, `¶`) and `data:` images are dropped.

mod article;
mod dom;
mod frontmatter;
mod markdown;
mod metadata;
mod patterns;
mod score;

use std::fmt;

use chrono::NaiveDate;
use sha2::{Digest, Sha256};

pub use url::Url;

/// How the page's HTML was obtained; recorded in the frontmatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FetchPath {
    /// The live page.
    Live,
    /// Raindrop's stored copy of the page.
    RaindropCache,
    /// A Wayback Machine snapshot.
    Wayback,
}

impl FetchPath {
    /// The frontmatter and ledger spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            FetchPath::Live => "live",
            FetchPath::RaindropCache => "raindrop_cache",
            FetchPath::Wayback => "wayback",
        }
    }
}

impl fmt::Display for FetchPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the pipeline knows about a page besides its HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceMeta {
    /// The bookmarked URL: the base for relative links, and `source:`.
    pub source: Url,
    /// The clip date (`clipped:`), passed in so clipping stays pure.
    pub clipped: NaiveDate,
    /// How the HTML was fetched.
    pub fetch_path: FetchPath,
    /// Raindrop's bookmark id, when the page came from Raindrop.
    pub raindrop_id: Option<u64>,
    /// When the bookmark was saved.
    pub saved: Option<NaiveDate>,
    /// The bookmark's own title, used when the page has no `<title>`.
    pub title: Option<String>,
    /// The bookmark's tags; page keywords are appended after them.
    pub tags: Vec<String>,
    /// Highlights made on the bookmark, verbatim.
    pub highlights: Vec<String>,
}

/// A clipped page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipped {
    /// The page title (or the bookmark title when the page has none).
    pub title: String,
    /// The YAML frontmatter, including its `---` fences.
    pub frontmatter: String,
    /// The article as markdown.
    pub body: String,
    /// Lowercase hex SHA-256 of [`Clipped::body`]; also in the frontmatter.
    pub content_hash: String,
}

impl Clipped {
    /// The whole note: frontmatter, a blank line, then the body.
    pub fn markdown(&self) -> String {
        format!("{}\n{}", self.frontmatter, self.body)
    }
}

/// Why a page could not be clipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipError {
    /// No article was found: no paragraph scored (a script-rendered
    /// page, an empty document) or the article rendered to nothing.
    NoContent,
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClipError::NoContent => f.write_str("no article content found in the page"),
        }
    }
}

impl std::error::Error for ClipError {}

/// Clips `html` to markdown with frontmatter.
///
/// # Errors
/// [`ClipError::NoContent`] when the page has no extractable article.
pub fn clip(html: &str, meta: &SourceMeta) -> Result<Clipped, ClipError> {
    let mut dom = dom::Dom::parse_and_clean(html);
    let page = metadata::extract(&dom.doc);
    let scores = score::score_paragraphs(&dom);
    let roots = article::select_roots(&dom, &scores).ok_or(ClipError::NoContent)?;
    article::final_clean(&mut dom, &roots);
    let body = markdown::render(&dom, &roots, &meta.source);
    if body.is_empty() {
        return Err(ClipError::NoContent);
    }
    let content_hash = sha256_hex(&body);
    let title = choose_title(&page.title, meta.title.as_deref());
    let frontmatter = frontmatter::render(&title, &page, meta, &content_hash);
    Ok(Clipped {
        title,
        frontmatter,
        body,
        content_hash,
    })
}

fn choose_title(page_title: &str, bookmark_title: Option<&str>) -> String {
    if !page_title.is_empty() {
        return page_title.to_string();
    }
    bookmark_title
        .map(metadata::collapse_whitespace)
        .unwrap_or_default()
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_falls_back_to_the_bookmark() {
        assert_eq!(choose_title("Page", Some("Bookmark")), "Page");
        assert_eq!(choose_title("", Some(" Book  mark ")), "Book mark");
        assert_eq!(choose_title("", None), "");
    }

    #[test]
    fn sha256_of_empty_is_the_known_digest() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
