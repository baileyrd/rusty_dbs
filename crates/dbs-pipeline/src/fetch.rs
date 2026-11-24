//! Fetch port: the live page, then Raindrop's stored copy, then the
//! Wayback Machine. Every attempt is kept, so a success says which path
//! worked and a failure carries the whole chain.

use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use dbs_clip::FetchPath;
use dbs_core::ManagedHttpClient;
use serde_json::Value;
use url::Url;

use crate::source::{Bookmark, RaindropApi};

/// Pages larger than this are refused before the body is read.
const MAX_PAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Redirect hops followed for one page (share.google takes two).
const MAX_REDIRECTS: usize = 10;
/// A generic client name; no personal details.
const USER_AGENT: &str = "rusty_dbs-pipeline/0.1";

/// What a fetch returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Html(String),
    /// Plain text, such as Raindrop's flat-text stored copy.
    Text(String),
    /// A PDF: recorded as such, not clipped.
    Pdf,
}

/// One failed step of the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub path: FetchPath,
    pub error: String,
}

impl fmt::Display for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.error)
    }
}

/// A successful fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub path: FetchPath,
    /// The URL the content came from, after redirects.
    pub url: Url,
    pub content: Content,
    /// Steps that failed before this one succeeded.
    pub failed_before: Vec<Attempt>,
}

/// Every step failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchFailure {
    pub attempts: Vec<Attempt>,
}

impl fmt::Display for FetchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chain: Vec<String> = self.attempts.iter().map(Attempt::to_string).collect();
        f.write_str(&chain.join("; "))
    }
}

/// Where a bookmark's stored copy lives (the Raindrop API in practice).
pub trait CopyStore {
    fn copy_location(&self, id: u64) -> Result<Option<Url>, String>;
}

/// Shared between workers: Raindrop's rate limit is per user, so one
/// limiter guards every worker's calls.
impl CopyStore for Mutex<RaindropApi> {
    fn copy_location(&self, id: u64) -> Result<Option<Url>, String> {
        let mut api = self
            .lock()
            .map_err(|_| "Raindrop client lock poisoned".to_string())?;
        api.cached_copy_location(id).map_err(|e| e.to_string())
    }
}

/// Runs the fallback chain for one bookmark at a time.
pub struct Fetcher<'a, C: CopyStore + ?Sized> {
    pages: ManagedHttpClient,
    copies: &'a C,
    wayback_base: Url,
}

impl<'a, C: CopyStore + ?Sized> Fetcher<'a, C> {
    /// `pages` fetches pages and snapshots (see [`page_http_client`]);
    /// `wayback_base` is `https://archive.org/` outside tests.
    pub fn new(pages: ManagedHttpClient, copies: &'a C, wayback_base: Url) -> Self {
        Self {
            pages,
            copies,
            wayback_base,
        }
    }

    /// Live page, then stored copy, then Wayback.
    pub fn fetch(&mut self, bookmark: &Bookmark) -> Result<Fetched, FetchFailure> {
        let mut attempts = Vec::new();
        for path in [
            FetchPath::Live,
            FetchPath::RaindropCache,
            FetchPath::Wayback,
        ] {
            match self.try_path(path, bookmark) {
                Ok((url, content)) => {
                    return Ok(Fetched {
                        path,
                        url,
                        content,
                        failed_before: attempts,
                    })
                }
                Err(error) => attempts.push(Attempt { path, error }),
            }
        }
        Err(FetchFailure { attempts })
    }

    fn try_path(&mut self, path: FetchPath, bookmark: &Bookmark) -> Result<(Url, Content), String> {
        match path {
            FetchPath::Live => self.get_page(&bookmark.link),
            FetchPath::RaindropCache => self.stored_copy(bookmark.id),
            FetchPath::Wayback => self.wayback(&bookmark.link),
        }
    }

    fn stored_copy(&mut self, id: u64) -> Result<(Url, Content), String> {
        let location = self
            .copies
            .copy_location(id)?
            .ok_or_else(|| "no stored copy".to_string())?;
        // A plain request: the Raindrop token never reaches the storage host.
        self.get_page(&location)
    }

    fn wayback(&mut self, link: &Url) -> Result<(Url, Content), String> {
        let mut api = self
            .wayback_base
            .join("wayback/available")
            .map_err(|e| format!("bad Wayback base: {e}"))?;
        api.query_pairs_mut().append_pair("url", link.as_str());
        let body: Value = self
            .pages
            .get(api.as_str())
            .map_err(|e| e.to_string())?
            .json()
            .map_err(|e| format!("invalid Wayback availability JSON: {e}"))?;
        let snapshot = closest_snapshot(&body).ok_or_else(|| "no Wayback snapshot".to_string())?;
        self.get_page(&raw_snapshot_url(&snapshot))
    }

    fn get_page(&mut self, url: &Url) -> Result<(Url, Content), String> {
        let response = self.pages.get(url.as_str()).map_err(|e| e.to_string())?;
        let final_url = response.url().clone();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = response.bytes().map_err(|e| format!("reading body: {e}"))?;
        Ok((final_url, classify(&content_type, &bytes)?))
    }
}

/// Decides what a body is from its type and first bytes.
pub fn classify(content_type: &str, body: &[u8]) -> Result<Content, String> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err("empty body".to_string());
    }
    if content_type.contains("application/pdf") || body.starts_with(b"%PDF-") {
        return Ok(Content::Pdf);
    }
    let text = String::from_utf8_lossy(body).into_owned();
    if content_type.contains("html") || (content_type.is_empty() && looks_like_html(&text)) {
        return Ok(Content::Html(text));
    }
    if content_type.starts_with("text/") || content_type.is_empty() {
        return Ok(Content::Text(text));
    }
    Err(format!("unsupported content type {content_type}"))
}

fn looks_like_html(text: &str) -> bool {
    let head = text
        .trim_start()
        .get(..64)
        .unwrap_or(text.trim_start())
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.starts_with('<')
}

/// The usable closest snapshot from Wayback's availability answer.
fn closest_snapshot(body: &Value) -> Option<Url> {
    let closest = body.get("archived_snapshots")?.get("closest")?;
    let available = closest.get("available").and_then(Value::as_bool) == Some(true);
    let ok = closest.get("status").and_then(Value::as_str) == Some("200");
    if !(available && ok) {
        return None;
    }
    Url::parse(closest.get("url")?.as_str()?).ok()
}

/// `…/web/20150101000000/https://x` becomes `…/web/20150101000000id_/https://x`,
/// Wayback's raw form without its toolbar and rewritten links.
pub fn raw_snapshot_url(snapshot: &Url) -> Url {
    let text = snapshot.as_str();
    let Some(start) = text.find("/web/").map(|i| i + "/web/".len()) else {
        return snapshot.clone();
    };
    let stamp_len = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    if stamp_len == 0 || text[start + stamp_len..].starts_with("id_") {
        return snapshot.clone();
    }
    let raw = format!(
        "{}id_{}",
        &text[..start + stamp_len],
        &text[start + stamp_len..]
    );
    Url::parse(&raw).unwrap_or_else(|_| snapshot.clone())
}

/// A client for pages: follows redirects (share.google links redirect
/// twice), two attempts, a 20 MB cap and a timeout.
pub fn page_http_client(timeout: Duration) -> Result<ManagedHttpClient, String> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .timeout(timeout)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))?;
    Ok(ManagedHttpClient::new(client)
        .max_attempts(2)
        .max_response_bytes(MAX_PAGE_BYTES))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_by_type_and_magic() {
        assert_eq!(classify("application/pdf", b"x"), Ok(Content::Pdf));
        assert_eq!(classify("", b"%PDF-1.7"), Ok(Content::Pdf));
        assert_eq!(
            classify("text/html; charset=utf-8", b"<p>x</p>"),
            Ok(Content::Html("<p>x</p>".into()))
        );
        assert_eq!(
            classify("", b"  <!DOCTYPE html><p>x"),
            Ok(Content::Html("  <!DOCTYPE html><p>x".into()))
        );
        assert_eq!(
            classify("text/plain", b"flat"),
            Ok(Content::Text("flat".into()))
        );
        assert_eq!(classify("text/html", b" \n "), Err("empty body".into()));
        assert!(classify("image/png", b"\x89PNG").is_err());
    }

    #[test]
    fn raw_snapshot_url_inserts_id_once() {
        let snap =
            Url::parse("http://web.archive.org/web/20150701000000/http://lisp.example/m.html")
                .expect("url");
        assert_eq!(
            raw_snapshot_url(&snap).as_str(),
            "http://web.archive.org/web/20150701000000id_/http://lisp.example/m.html"
        );
        let already = raw_snapshot_url(&snap);
        assert_eq!(raw_snapshot_url(&already), already);
    }

    #[test]
    fn closest_snapshot_needs_available_and_200() {
        let ok = json!({"archived_snapshots": {"closest": {"available": true, "status": "200", "url": "http://web.archive.org/web/1/http://a"}}});
        assert!(closest_snapshot(&ok).is_some());
        let gone = json!({"archived_snapshots": {"closest": {"available": true, "status": "404", "url": "http://web.archive.org/web/1/http://a"}}});
        assert!(closest_snapshot(&gone).is_none());
        assert!(closest_snapshot(&json!({"archived_snapshots": {}})).is_none());
    }
}
