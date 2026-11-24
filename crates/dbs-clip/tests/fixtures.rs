//! Fixture pages, one per kind in #763's first batch: an article with
//! boilerplate, a GitHub repo page, a gist with a comment thread, an
//! arXiv abstract page, a redirect target (share.google), a
//! script-only page and a 2015 table-and-`<br>` page.
//!
//! The fixtures are hand-made, modeled on the real pages' structure, so
//! the expected output is stable and the files stay small. The batch's
//! own fetched pages are captured by the first batch run (#1310).

use chrono::NaiveDate;
use dbs_clip::{clip, ClipError, Clipped, FetchPath, SourceMeta, Url};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}.html", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn meta(source: &str) -> SourceMeta {
    SourceMeta {
        source: Url::parse(source).expect("fixture url"),
        clipped: NaiveDate::from_ymd_opt(2026, 11, 24).expect("date"),
        fetch_path: FetchPath::Live,
        raindrop_id: Some(1914904474),
        saved: NaiveDate::from_ymd_opt(2026, 11, 2),
        title: Some("Bookmark title".to_string()),
        tags: vec!["Rust".to_string()],
        highlights: vec!["the owner decides when the value is dropped".to_string()],
    }
}

fn clip_fixture(name: &str, source: &str) -> Clipped {
    clip(&fixture(name), &meta(source)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn assert_contains_all(body: &str, landmarks: &[&str]) {
    for landmark in landmarks {
        assert!(body.contains(landmark), "missing {landmark:?} in:\n{body}");
    }
}

fn assert_contains_none(body: &str, absent: &[&str]) {
    for text in absent {
        assert!(!body.contains(text), "unexpected {text:?} in:\n{body}");
    }
}

#[test]
fn article_frontmatter_snapshot() {
    let clipped = clip_fixture("article", "https://example.com/blog/ownership");
    let expected = format!(
        "---
category: \"[[Clippings]]\"
author: \"[[Ada Lovelace]]\"
title: \"Why Ownership Matters: A Field Guide | Example Blog\"
description: \"How ownership shapes Rust APIs, with examples.\"
source: \"https://example.com/blog/ownership\"
clipped: 2026-11-24
published: 2026-10-30
tags:
  - \"Rust\"
  - \"ownership\"
  - \"api design\"
raindrop_id: 1914904474
saved: 2026-11-02
highlights:
  - \"the owner decides when the value is dropped\"
fetch_path: live
content_hash: {}
---
",
        clipped.content_hash
    );
    assert_eq!(clipped.frontmatter, expected);
    assert_eq!(
        clipped.title,
        "Why Ownership Matters: A Field Guide | Example Blog"
    );
}

#[test]
fn article_keeps_content_and_drops_boilerplate() {
    let body = clip_fixture("article", "https://example.com/blog/ownership").body;
    assert_contains_all(
        &body,
        &[
            "## Introduction\n",
            "takes a `String` by value",
            "![Borrow diagram](https://example.com/img/borrow.png)",
            "- Ownership can move, *explicitly*.\n    - Moves are cheap.",
            "```rust\nfn consume(s: String) {\n    println!(\"{s}\");\n}\n```",
            "> Make illegal states unrepresentable",
            "[the borrowing post](https://example.com/posts/borrowing)",
        ],
    );
    assert_contains_none(
        &body,
        &[
            "Home",
            "5 min read",
            "Share on Twitter",
            "Great post",
            "newsletter",
            "Copyright",
            "analytics",
            "[#]",
        ],
    );
}

#[test]
fn github_repo_page_yields_the_readme() {
    let body = clip_fixture("github_repo", "https://github.com/example/fastgrep").body;
    assert!(
        body.starts_with("# fastgrep\n\nfastgrep is a line-oriented search tool"),
        "{body}"
    );
    assert_contains_all(
        &body,
        &[
            "## Installation\n\n```\n$ cargo install fastgrep\n$ fg --version\n```",
            "| Tool | Time |\n| --- | --- |\n| fastgrep | 0.08s |",
            "(https://github.com/example/fastgrep/blob/main/BENCHMARKS.md)",
        ],
    );
    assert_contains_none(
        &body,
        &[
            "Pricing",
            "Bump version",
            "Licensed under MIT",
            "Terms, Privacy",
        ],
    );
}

#[test]
fn gist_yields_the_gist_not_its_comments() {
    let body = clip_fixture(
        "gist_with_comments",
        "https://gist.github.com/example/abc123",
    )
    .body;
    assert_contains_all(&body, &["# LLM Notes", "knowledge accumulates"]);
    assert_contains_none(&body, &["Inspired by this", "rerankers", "team of five"]);
}

#[test]
fn arxiv_abstract_page_yields_the_abstract() {
    let body = clip_fixture("arxiv_abstract", "https://arxiv.org/abs/2501.00001").body;
    assert_contains_all(
        &body,
        &[
            "# Title:Sparse Mixtures for Long Context",
            "[Jane Doe](https://arxiv.org/a/doe_j_1)",
            "> Abstract:We study sparse mixtures of experts",
        ],
    );
    assert_contains_none(&body, &["arXivLabs", "View PDF"]);
}

#[test]
fn redirect_target_links_resolve_against_the_final_url() {
    let body = clip_fixture("redirect_target", "https://opsweekly.example/2026/cron").body;
    assert_contains_all(
        &body,
        &[
            "# The quiet return of the cron job",
            "[field notes](https://opsweekly.example/2026/notes/cron.html)",
        ],
    );
}

#[test]
fn script_only_page_is_a_clear_no_content_error() {
    let result = clip(&fixture("script_only"), &meta("https://app.example/"));
    assert_eq!(result, Err(ClipError::NoContent));
    assert_eq!(
        ClipError::NoContent.to_string(),
        "no article content found in the page"
    );
}

#[test]
fn empty_input_is_no_content() {
    assert_eq!(
        clip("", &meta("https://example.com/")),
        Err(ClipError::NoContent)
    );
}

#[test]
fn old_table_layout_page_keeps_its_paragraphs() {
    let clipped = clip_fixture("old_table_layout_2015", "http://lisp.example/macros.html");
    assert_eq!(
        clipped.body,
        "March 2015\n\nMacros are programs that write programs, and in Lisp they are ordinary functions that run at compile time, which is why they feel so natural.\n\nMost languages make you choose between power and readability. Lisp lets you build the readability yourself, one macro at a time, with care.\n\n**Thanks** to the readers of [earlier drafts](http://lisp.example/drafts.html).\n"
    );
    assert_contains_none(&clipped.frontmatter, &["author: \""]);
}

#[test]
fn clipping_is_deterministic() {
    let first = clip_fixture("article", "https://example.com/blog/ownership");
    let second = clip_fixture("article", "https://example.com/blog/ownership");
    assert_eq!(first, second);
}

#[test]
fn content_hash_is_sha256_of_the_body_and_pinned() {
    let clipped = clip_fixture("old_table_layout_2015", "http://lisp.example/macros.html");
    assert_eq!(
        clipped.content_hash,
        "044996169a849f7aa420acd99957c863d6b99479bafbfdb0beef8d6646544124"
    );
    assert!(clipped
        .frontmatter
        .contains(&format!("content_hash: {}\n", clipped.content_hash)));
}

#[test]
fn the_same_page_saved_twice_hashes_the_same() {
    // Raindrop ids 1902093857 and 1900852003 are one article saved twice:
    // the body hash is what dedupes them; the frontmatter differs.
    let html = fixture("article");
    let mut first = meta("https://example.com/blog/ownership");
    first.raindrop_id = Some(1902093857);
    let mut second = meta("https://example.com/blog/ownership?utm_source=feed");
    second.raindrop_id = Some(1900852003);
    second.fetch_path = FetchPath::Wayback;
    let a = clip(&html, &first).expect("first");
    let b = clip(&html, &second).expect("second");
    assert_eq!(a.content_hash, b.content_hash);
    assert_ne!(a.frontmatter, b.frontmatter);
    assert!(b.frontmatter.contains("fetch_path: wayback\n"));
}

#[test]
fn markdown_joins_frontmatter_and_body() {
    let clipped = clip_fixture("redirect_target", "https://opsweekly.example/2026/cron");
    assert_eq!(
        clipped.markdown(),
        format!("{}\n{}", clipped.frontmatter, clipped.body)
    );
    assert!(clipped.markdown().contains("---\n\n# The quiet return"));
}
