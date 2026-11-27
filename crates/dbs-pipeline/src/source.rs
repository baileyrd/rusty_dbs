//! Source port and its Raindrop adapter.
//!
//! The backup connector (`dbs-connector-raindrop`) pages collections for
//! the engine's incremental/reconcile/full modes and yields engine items;
//! the pipeline needs something narrower (the newest N bookmarks, or
//! given ids, with highlights), so this adapter talks to the same API
//! through the same [`ManagedHttpClient`] (retry, `Retry-After`, rate
//! limit) and validates each bookmark at the boundary.

use chrono::{DateTime, NaiveDate};
use dbs_core::{HttpError, ManagedHttpClient};
use serde_json::Value;
use url::Url;

/// Raindrop allows 120 requests a minute per user.
pub const RAINDROP_REQUESTS_PER_MIN: u32 = 120;
/// Raindrop's largest page.
const PAGE_SIZE: usize = 50;
/// The "all collections except Trash" pseudo-collection.
const ALL_COLLECTIONS: i64 = 0;

/// A validated bookmark: what the pipeline needs, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub id: u64,
    pub link: Url,
    pub title: String,
    pub tags: Vec<String>,
    pub saved: Option<NaiveDate>,
    pub highlights: Vec<String>,
}

/// A bookmark the source returned but the pipeline cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// The id, when the item had a usable one.
    pub id: Option<u64>,
    pub reason: String,
}

/// What a listing produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Listing {
    pub bookmarks: Vec<Bookmark>,
    pub rejected: Vec<Rejected>,
}

/// Why a source call failed as a whole.
#[derive(Debug)]
pub enum SourceError {
    /// The token was refused (401/403).
    Auth(String),
    /// Any other HTTP or decoding failure.
    Http(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Auth(m) => write!(f, "authentication failed: {m}"),
            SourceError::Http(m) => write!(f, "source request failed: {m}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// The source port: where bookmarks come from.
pub trait BookmarkSource {
    /// The newest `n` bookmarks, newest first.
    fn newest(&mut self, n: usize) -> Result<Listing, SourceError>;
    /// The given bookmarks, in the order asked.
    fn by_ids(&mut self, ids: &[u64]) -> Result<Listing, SourceError>;
}

/// Raindrop REST API v1 client: bookmarks and their stored copies.
///
/// Holds the bearer token; it is sent only to `base_url` and never
/// logged or written anywhere.
pub struct RaindropApi {
    http: ManagedHttpClient,
    base_url: Url,
    token: String,
}

impl RaindropApi {
    /// `http` must not follow redirects: [`Self::cached_copy_location`]
    /// reads the redirect itself so the token never reaches the storage
    /// host. Use [`api_http_client`].
    pub fn new(http: ManagedHttpClient, base_url: Url, token: String) -> Self {
        Self {
            http,
            base_url,
            token,
        }
    }

    /// Where Raindrop keeps its stored copy of bookmark `id`, or `None`
    /// when it has none. Raindrop answers `/raindrop/{id}/cache` with a
    /// redirect to a storage URL when a copy exists (DBS's
    /// `archive_permanent_copy`, Daily-Backup-System@6cc6491). An error
    /// (a 403 on a non-Pro account, a 404) is returned for the caller to
    /// record; it is not fatal to a batch.
    pub fn cached_copy_location(&mut self, id: u64) -> Result<Option<Url>, SourceError> {
        let url = self.endpoint(&format!("rest/v1/raindrop/{id}/cache"))?;
        let response = self.get(&url)?;
        if !response.status().is_redirection() {
            return Ok(None);
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok());
        Ok(location.and_then(|l| url.join(l).ok()))
    }

    fn endpoint(&self, path: &str) -> Result<Url, SourceError> {
        self.base_url
            .join(path)
            .map_err(|e| SourceError::Http(format!("bad endpoint {path}: {e}")))
    }

    fn get(&mut self, url: &Url) -> Result<reqwest::blocking::Response, SourceError> {
        let token = &self.token;
        self.http
            .request(reqwest::Method::GET, url.as_str(), |b| b.bearer_auth(token))
            .map_err(classify)
    }

    fn get_json(&mut self, url: &Url) -> Result<Value, SourceError> {
        self.get(url)?
            .json()
            .map_err(|e| SourceError::Http(format!("invalid JSON from {}: {e}", url.path())))
    }

    fn page(&mut self, page: usize) -> Result<Vec<Value>, SourceError> {
        let mut url = self.endpoint(&format!("rest/v1/raindrops/{ALL_COLLECTIONS}"))?;
        url.query_pairs_mut()
            .append_pair("sort", "-created")
            .append_pair("perpage", &PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        let body = self.get_json(&url)?;
        Ok(body
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

impl BookmarkSource for RaindropApi {
    fn newest(&mut self, n: usize) -> Result<Listing, SourceError> {
        let mut listing = Listing::default();
        let mut page = 0;
        while listing.bookmarks.len() + listing.rejected.len() < n {
            let items = self.page(page)?;
            let last_page = items.len() < PAGE_SIZE;
            let wanted = n - listing.bookmarks.len() - listing.rejected.len();
            for raw in items.iter().take(wanted) {
                push_parsed(&mut listing, raw);
            }
            if last_page {
                break;
            }
            page += 1;
        }
        Ok(listing)
    }

    fn by_ids(&mut self, ids: &[u64]) -> Result<Listing, SourceError> {
        let mut listing = Listing::default();
        for id in ids {
            let url = self.endpoint(&format!("rest/v1/raindrop/{id}"))?;
            match self.get_json(&url) {
                Ok(body) => push_parsed(&mut listing, body.get("item").unwrap_or(&Value::Null)),
                Err(SourceError::Http(reason)) => listing.rejected.push(Rejected {
                    id: Some(*id),
                    reason,
                }),
                Err(e) => return Err(e),
            }
        }
        Ok(listing)
    }
}

/// A blocking client for the Raindrop API: no redirects (see
/// [`RaindropApi::cached_copy_location`]), rate limited to 120/min.
pub fn api_http_client(timeout: std::time::Duration) -> Result<ManagedHttpClient, SourceError> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|e| SourceError::Http(format!("cannot build HTTP client: {e}")))?;
    Ok(ManagedHttpClient::new(client).rate_limit_per_min(RAINDROP_REQUESTS_PER_MIN))
}

fn classify(e: HttpError) -> SourceError {
    if let HttpError::Status { error, .. } = &e {
        if matches!(error.status().map(|s| s.as_u16()), Some(401 | 403)) {
            return SourceError::Auth(e.to_string());
        }
    }
    SourceError::Http(e.to_string())
}

fn push_parsed(listing: &mut Listing, raw: &Value) {
    match parse_bookmark(raw) {
        Ok(b) => listing.bookmarks.push(b),
        Err(r) => listing.rejected.push(r),
    }
}

/// Validates one Raindrop item: a positive id and an http(s) link are
/// required; everything else is optional and defaults to empty.
pub fn parse_bookmark(raw: &Value) -> Result<Bookmark, Rejected> {
    let id = raw.get("_id").and_then(Value::as_u64).filter(|id| *id > 0);
    let reject = |reason: &str| Rejected {
        id,
        reason: reason.to_string(),
    };
    let id = id.ok_or_else(|| reject("missing or invalid _id"))?;
    let link = raw
        .get("link")
        .and_then(Value::as_str)
        .and_then(|l| Url::parse(l.trim()).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .ok_or_else(|| reject("missing or non-http(s) link"))?;
    Ok(Bookmark {
        id,
        link,
        title: str_field(raw, "title"),
        tags: strings(raw.get("tags")),
        saved: raw
            .get("created")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.date_naive()),
        highlights: highlights(raw.get("highlights")),
    })
}

fn str_field(raw: &Value, key: &str) -> String {
    raw.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string()
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Highlight texts (Raindrop's `highlights[].text`), notes left out.
fn highlights(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|h| h.get("text").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_full_item() {
        let b = parse_bookmark(&json!({
            "_id": 1914904474u64,
            "link": " https://example.com/a ",
            "title": " A title ",
            "tags": ["rust", " ", "llm"],
            "created": "2026-11-02T10:00:00.000Z",
            "highlights": [{"text": "key line", "note": "mine"}, {"text": ""}]
        }))
        .expect("valid");
        assert_eq!(b.id, 1914904474);
        assert_eq!(b.link.as_str(), "https://example.com/a");
        assert_eq!(b.title, "A title");
        assert_eq!(b.tags, ["rust", "llm"]);
        assert_eq!(b.saved, NaiveDate::from_ymd_opt(2026, 11, 2));
        assert_eq!(b.highlights, ["key line"]);
    }

    #[test]
    fn rejects_bad_ids_and_links_at_the_boundary() {
        let no_id = parse_bookmark(&json!({"link": "https://e.com"})).unwrap_err();
        assert_eq!(no_id.id, None);
        let bad_link =
            parse_bookmark(&json!({"_id": 5, "link": "javascript:alert(1)"})).unwrap_err();
        assert_eq!(bad_link.id, Some(5));
        assert!(parse_bookmark(&json!({"_id": -1, "link": "https://e.com"})).is_err());
    }
}
