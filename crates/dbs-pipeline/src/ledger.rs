//! Append-only JSONL ledger: one line per settled bookmark, so a batch
//! resumes where it stopped and never refetches a done id.
//!
//! Each line is written whole and synced before the next item starts.
//! A crash can leave at most one torn last line; loading skips it (and
//! reports it), and the next append starts on a fresh line.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use dbs_clip::FetchPath;
use serde::{Deserialize, Serialize};
use url::Url;

/// How a bookmark settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Clipped and written.
    Done,
    /// Every fetch path failed, or clipping failed; retried next run.
    Failed,
    /// Deliberately not clipped (duplicate, PDF, invalid link); final.
    Skipped,
}

/// One ledger line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: u64,
    pub status: Status,
    /// The bookmark's link as saved.
    pub url: String,
    #[serde(
        default,
        with = "fetch_path_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub fetch_path: Option<FetchPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub at: DateTime<Utc>,
    /// Why it failed or was skipped (the whole fetch chain on failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<u64>,
}

/// What loading found.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LoadReport {
    pub entries: usize,
    /// Lines that did not parse (a torn last line after a crash).
    pub unreadable: usize,
}

/// The ledger file plus the state replayed from it.
pub struct Ledger {
    /// `None` for a read-only (dry-run) ledger.
    file: Option<File>,
    settled: HashMap<u64, Status>,
    /// Normalised URL to the id that owns it (done, or claimed this run).
    url_owner: HashMap<String, u64>,
    /// Body hash to the done id that has it.
    hash_owner: HashMap<String, u64>,
}

impl Ledger {
    /// Opens (creating if needed) the ledger for appending.
    pub fn open(path: &Path) -> io::Result<(Self, LoadReport)> {
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        if !text.is_empty() && !text.ends_with('\n') {
            file.write_all(b"\n")?;
            file.sync_data()?;
        }
        let (mut ledger, report) = Self::replay(&text);
        ledger.file = Some(file);
        Ok((ledger, report))
    }

    /// Loads the ledger without writing (a missing file is empty).
    pub fn read_only(path: &Path) -> io::Result<(Self, LoadReport)> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::replay(&text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::replay("")),
            Err(e) => Err(e),
        }
    }

    fn replay(text: &str) -> (Self, LoadReport) {
        let mut ledger = Ledger {
            file: None,
            settled: HashMap::new(),
            url_owner: HashMap::new(),
            hash_owner: HashMap::new(),
        };
        let mut report = LoadReport::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Entry>(line) {
                Ok(entry) => {
                    ledger.apply(&entry);
                    report.entries += 1;
                }
                Err(_) => report.unreadable += 1,
            }
        }
        (ledger, report)
    }

    /// Later lines win: a failed id that later succeeds is done. A failed
    /// id gives up the URL it claimed, so a later save of the same page
    /// can still be tried.
    fn apply(&mut self, entry: &Entry) {
        self.settled.insert(entry.id, entry.status);
        if entry.status == Status::Failed {
            self.url_owner.retain(|_, owner| *owner != entry.id);
        }
        if entry.status != Status::Done {
            return;
        }
        if let Some(norm) = Url::parse(&entry.url).ok().map(|u| normalize_url(&u)) {
            self.url_owner.entry(norm).or_insert(entry.id);
        }
        if let Some(hash) = &entry.content_hash {
            self.hash_owner.entry(hash.clone()).or_insert(entry.id);
        }
    }

    /// Done and skipped ids are final; failed ones are retried.
    pub fn is_settled(&self, id: u64) -> bool {
        matches!(self.settled.get(&id), Some(Status::Done | Status::Skipped))
    }

    /// The id that already owns this URL (done earlier, or claimed this
    /// run), or `None` after claiming it for `id`.
    pub fn claim_url(&mut self, id: u64, url: &Url) -> Option<u64> {
        let norm = normalize_url(url);
        match self.url_owner.get(&norm) {
            Some(owner) if *owner != id => Some(*owner),
            Some(_) => None,
            None => {
                self.url_owner.insert(norm, id);
                None
            }
        }
    }

    /// The other done id with this body hash, if any.
    pub fn hash_owner(&self, id: u64, hash: &str) -> Option<u64> {
        self.hash_owner
            .get(hash)
            .copied()
            .filter(|owner| *owner != id)
    }

    /// Appends one line and syncs it to disk before returning.
    pub fn record(&mut self, entry: &Entry) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("ledger opened read-only"))?;
        let mut line = serde_json::to_string(entry).map_err(io::Error::other)?;
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.sync_data()?;
        self.apply(entry);
        Ok(())
    }
}

/// Tracking parameters dropped when comparing URLs.
const TRACKING_PARAMS: &[&str] = &["fbclid", "gclid", "mc_cid", "mc_eid", "ref", "ref_src"];

/// A comparison key for "the same page": scheme, `www.`, fragment,
/// tracking parameters and a trailing slash do not count.
pub fn normalize_url(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = url.path();
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        ""
    };
    let query: Vec<String> = url
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !TRACKING_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let query = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    format!("//{host}{port}{path}{query}")
}

mod fetch_path_serde {
    use dbs_clip::FetchPath;
    use serde::{Deserialize, Deserializer, Serializer};

    const ALL: [FetchPath; 3] = [
        FetchPath::Live,
        FetchPath::RaindropCache,
        FetchPath::Wayback,
    ];

    pub fn serialize<S: Serializer>(path: &Option<FetchPath>, s: S) -> Result<S::Ok, S::Error> {
        match path {
            Some(p) => s.serialize_str(p.as_str()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<FetchPath>, D::Error> {
        let Some(text) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        ALL.into_iter()
            .find(|p| p.as_str() == text)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown fetch_path {text:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url")
    }

    #[test]
    fn normalize_ignores_noise_but_keeps_meaningful_query() {
        let a = normalize_url(&url("https://www.Example.com/post/?utm_source=x&id=7#top"));
        let b = normalize_url(&url("http://example.com/post?id=7&fbclid=abc"));
        assert_eq!(a, b);
        assert_eq!(a, "//example.com/post?id=7");
        assert_ne!(a, normalize_url(&url("https://example.com/post?id=8")));
        assert_eq!(normalize_url(&url("https://example.com/")), "//example.com");
    }

    #[test]
    fn entry_round_trips_and_omits_empty_fields() {
        let entry = Entry {
            id: 1,
            status: Status::Done,
            url: "https://e.com/a".into(),
            fetch_path: Some(FetchPath::RaindropCache),
            content_hash: Some("ab".into()),
            at: DateTime::parse_from_rfc3339("2026-11-24T08:00:00Z")
                .expect("time")
                .to_utc(),
            reason: None,
            duplicate_of: None,
        };
        let line = serde_json::to_string(&entry).expect("json");
        assert_eq!(
            line,
            r#"{"id":1,"status":"done","url":"https://e.com/a","fetch_path":"raindrop_cache","content_hash":"ab","at":"2026-11-24T08:00:00Z"}"#
        );
        assert_eq!(serde_json::from_str::<Entry>(&line).expect("parse"), entry);
        assert!(serde_json::from_str::<Entry>(&line.replace("raindrop_cache", "ftp")).is_err());
    }
}
