//! GitHub connector: backs up your starred repositories and gists
//! (issue #86). Mirrors `dbs.connectors.github` in
//! baileyrd/Daily-Backup-System.
//!
//! A clean template-A source (REST + token + a real incremental
//! cursor, like `raindrop`) with one asymmetry worth noting:
//!
//! * **Stars** have no server-side `since` filter, but the listing
//!   sorts by when *you* starred (`sort=created&direction=desc` with
//!   the `application/vnd.github.star+json` media type, which adds
//!   `starred_at` to each entry). Incremental mode pages newest-first
//!   and early-stops once `starred_at` drops below the stored
//!   high-water mark — `raindrop`'s exact fast path.
//! * **Gists** DO have a real delta filter (`GET /gists?since=ISO`,
//!   matched against `updated_at`), so their incremental mode is a
//!   genuine server-side query.
//!
//! Identity uses immutable ids (`star:<repo id>`, `gist:<gist id>`) so
//! a repository rename never forks the item. `raw` holds the verbatim
//! API entry; for stars, the nested `repo` object is declared volatile
//! — GitHub mutates its counters (stargazers, forks, `pushed_at`, ...)
//! constantly, which would otherwise spawn a revision per reconcile
//! per repo. Meaningful changes (rename, description, topics,
//! language) still surface through the semantic projection
//! (title/url/body/tags), which IS hashed.
//!
//! Deletion detection: a full/reconcile run enumerates both kinds and
//! yields one [`ReconcileMarker`]. If either kind is disabled in
//! config the marker is withheld entirely — an enumeration that
//! deliberately skipped a kind must never offer that kind's stored
//! items up for sweeping.
//!
//! Auth: a personal access token (classic or fine-grained) in
//! `GITHUB_TOKEN`. No scopes needed for public data; `gist` (classic)
//! or Gists read (fine-grained) to include secret gists.
//!
//! Reachable from a real `dbs backup github` run since #164's
//! `dbs-connector-github` subprocess binary. Tested directly against
//! the `Connector` trait and fixture HTTP responses.

use std::cell::RefCell;
use std::collections::HashSet;

use dbs_connector_support::{step_fetch, EventQueue, Step};
use dbs_core::parse_iso;
use dbs_core::{
    BackupItem, Capabilities, Checkpoint, Connector, ConnectorError, Cursor, FetchEvent, ItemKind,
    ManagedHttpClient, ReconcileMarker, RunContext,
};
use serde_json::Value;

const DEFAULT_BASE_URL: &str = "https://api.github.com";
const STARS_ACCEPT: &str = "application/vnd.github.star+json";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";
/// Re-fetch window below the stored watermark, mirroring `raindrop`'s
/// overlap: clocks skew and pagination races; the idempotent upsert
/// dedups the overlap.
const OVERLAP_SECONDS: i64 = 300;

#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub include_stars: bool,
    pub include_gists: bool,
    /// API page size, max 100.
    pub page_size: u32,
    pub token_env: String,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            include_stars: true,
            include_gists: true,
            page_size: 100,
            token_env: "GITHUB_TOKEN".to_string(),
        }
    }
}

pub struct GitHubConnector {
    config: GitHubConfig,
    base_url: String,
    secret_keys: Vec<String>,
    item_kinds: Vec<ItemKind>,
    volatile_fields: Vec<String>,
}

impl GitHubConnector {
    pub fn new(config: GitHubConfig) -> Self {
        let token_env = config.token_env.clone();
        Self {
            config,
            base_url: DEFAULT_BASE_URL.to_string(),
            secret_keys: vec![token_env],
            item_kinds: vec![
                ItemKind {
                    name: "star".to_string(),
                    display_name: "Starred repository".to_string(),
                    description: String::new(),
                },
                ItemKind {
                    name: "gist".to_string(),
                    display_name: "Gist".to_string(),
                    description: String::new(),
                },
            ],
            // The nested repo object churns constantly (counters,
            // pushed_at, ...); semantic fields (title/url/body/tags)
            // still catch meaningful edits.
            volatile_fields: vec!["repo".to_string()],
        }
    }

    /// Overrides the API base URL (default `https://api.github.com`) —
    /// for tests to point at a local mock server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    fn get_json(
        &self,
        http: &RefCell<ManagedHttpClient>,
        url: &str,
        token: &str,
        accept: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ConnectorError> {
        let response = http
            .borrow_mut()
            .request(reqwest::Method::GET, url, |b| {
                b.bearer_auth(token)
                    .header("X-GitHub-Api-Version", API_VERSION)
                    .header("Accept", accept)
                    .query(params)
            })
            .map_err(classify_http_error)?;
        response
            .json()
            .map_err(|e| ConnectorError::Transient(format!("invalid JSON response: {e}")))
    }

    fn get_stars_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        page: u32,
    ) -> Result<Vec<Value>, ConnectorError> {
        let url = format!("{}/user/starred", self.base_url);
        let params = [
            ("sort", "created".to_string()),
            ("direction", "desc".to_string()),
            ("per_page", self.config.page_size.to_string()),
            ("page", page.to_string()),
        ];
        let value = self.get_json(http, &url, token, STARS_ACCEPT, &params)?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    fn get_gists_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        page: u32,
        since: Option<&str>,
    ) -> Result<Vec<Value>, ConnectorError> {
        let url = format!("{}/gists", self.base_url);
        let mut params = vec![
            ("per_page", self.config.page_size.to_string()),
            ("page", page.to_string()),
        ];
        if let Some(s) = since {
            params.push(("since", s.to_string()));
        }
        let value = self.get_json(http, &url, token, ACCEPT, &params)?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// Fetches one page of stars and queues its items, then a checkpoint
    /// carrying the cursor as it stood when the phase began. Returns
    /// whether another page should be fetched.
    ///
    /// Called once per page by `fetch`'s [`step_fetch`] step, so each
    /// page reaches the host before the next request is made.
    fn stars_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        cursor: &serde_json::Map<String, Value>,
        state: &mut StarsState,
        live_ids: &mut HashSet<String>,
        out: &mut EventQueue,
    ) -> Result<bool, ConnectorError> {
        let batch = self.get_stars_page(http, token, state.page)?;
        if batch.is_empty() {
            return Ok(false);
        }
        let mut stop = false;
        for entry in &batch {
            let starred_at = entry
                .get("starred_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ts = parse_iso(Some(&starred_at));
            if let (Some(h), Some(t)) = (state.high, ts) {
                if t < h - chrono::Duration::seconds(OVERLAP_SECONDS) {
                    stop = true;
                    break;
                }
            }
            let Some(item) = star_item(entry) else {
                continue;
            };
            live_ids.insert(item.external_id().to_string());
            if state
                .max_seen
                .as_deref()
                .is_none_or(|m| starred_at.as_str() > m)
            {
                state.max_seen = Some(starred_at.clone());
            }
            out.push_back(Ok(FetchEvent::Item(item)));
        }
        // Mid-phase checkpoints carry the OLD watermark: advancing
        // it before the walk completes would let a crash skip the
        // gap between the old mark and the last committed page
        // forever.
        out.push_back(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: Value::Object(cursor.clone()),
            },
            note: format!("stars page {}", state.page),
        })));
        if stop || (batch.len() as u32) < self.config.page_size {
            return Ok(false);
        }
        state.page += 1;
        Ok(true)
    }

    /// Fetches one page of gists and queues its items, then a
    /// checkpoint. Returns whether another page should be fetched.
    ///
    /// Called once per page by `fetch`'s [`step_fetch`] step.
    fn gists_page(
        &self,
        http: &RefCell<ManagedHttpClient>,
        token: &str,
        cursor: &serde_json::Map<String, Value>,
        state: &mut GistsState,
        live_ids: &mut HashSet<String>,
        out: &mut EventQueue,
    ) -> Result<bool, ConnectorError> {
        let batch = self.get_gists_page(http, token, state.page, state.since.as_deref())?;
        if batch.is_empty() {
            return Ok(false);
        }
        for gist in &batch {
            let Some(gid) = gist_id(gist) else {
                continue;
            };
            let item = gist_item(gist, &gid);
            live_ids.insert(item.external_id().to_string());
            let updated = gist
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !updated.is_empty()
                && state
                    .max_seen
                    .as_deref()
                    .is_none_or(|m| updated.as_str() > m)
            {
                state.max_seen = Some(updated);
            }
            out.push_back(Ok(FetchEvent::Item(item)));
        }
        out.push_back(Ok(FetchEvent::Checkpoint(Checkpoint {
            cursor: Cursor {
                value: Value::Object(cursor.clone()),
            },
            note: format!("gists page {}", state.page),
        })));
        if (batch.len() as u32) < self.config.page_size {
            return Ok(false);
        }
        state.page += 1;
        Ok(true)
    }

    /// Queues what follows the last page of both kinds: on a full run,
    /// the combined reconcile marker, or a warning when a kind was
    /// deliberately skipped.
    fn finish(&self, full: bool, live_ids: HashSet<String>, out: &mut EventQueue) {
        if !full {
            return;
        }
        if self.config.include_stars && self.config.include_gists {
            out.push_back(Ok(FetchEvent::ReconcileMarker(ReconcileMarker::new(
                live_ids,
            ))));
        } else {
            // A deliberately-partial enumeration must never offer
            // the skipped kind's stored items up for the sweep.
            // No logger equivalent exists in `RunContext` yet
            // (same gap its own doc-comment calls out) — stderr
            // stands in, matching this port's other connector-side
            // warnings (e.g. `dbs-research::youtube_search`).
            eprintln!(
                "github: a kind is disabled (include_stars={}, include_gists={}) — deletion \
                 detection skipped",
                self.config.include_stars, self.config.include_gists
            );
        }
    }
}

/// Paging state for the stars phase.
struct StarsState {
    /// Incremental early-stop watermark; `None` on a full run.
    high: Option<chrono::DateTime<chrono::Utc>>,
    max_seen: Option<String>,
    page: u32,
}

impl StarsState {
    fn new(cursor: &serde_json::Map<String, Value>, full: bool) -> Self {
        let high = if full {
            None
        } else {
            cursor
                .get("stars_high_watermark")
                .and_then(|v| v.as_str())
                .and_then(|s| parse_iso(Some(s)))
        };
        let max_seen = cursor
            .get("stars_high_watermark")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Self {
            high,
            max_seen,
            page: 1,
        }
    }
}

/// Paging state for the gists phase.
struct GistsState {
    /// Server-side `since` filter; `None` on a full run.
    since: Option<String>,
    max_seen: Option<String>,
    page: u32,
}

impl GistsState {
    fn new(cursor: &serde_json::Map<String, Value>, full: bool) -> Self {
        let since = if full {
            None
        } else {
            cursor
                .get("gists_high_watermark")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let max_seen = cursor
            .get("gists_high_watermark")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Self {
            since,
            max_seen,
            page: 1,
        }
    }
}

/// Where a fetch is in its run.
enum Stage {
    Stars(StarsState),
    Gists(GistsState),
    /// Both kinds are done; queue the reconcile marker (if any).
    Finish,
    Done,
}

/// Ends a phase whose walk completed: advances that kind's watermark
/// to the newest timestamp seen and queues a checkpoint committing it.
fn finish_phase(
    cursor: &mut serde_json::Map<String, Value>,
    key: &str,
    max_seen: Option<String>,
    note: &str,
    out: &mut EventQueue,
) {
    let Some(seen) = max_seen else {
        return;
    };
    cursor.insert(key.to_string(), Value::String(seen));
    out.push_back(Ok(FetchEvent::Checkpoint(Checkpoint {
        cursor: Cursor {
            value: Value::Object(cursor.clone()),
        },
        note: note.to_string(),
    })));
}

fn star_item(entry: &Value) -> Option<BackupItem> {
    let repo = entry.get("repo").cloned().unwrap_or(Value::Null);
    let repo_id = repo.get("id")?;
    let repo_id_str = match repo_id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    let mut topics: Vec<String> = repo
        .get("topics")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if let Some(lang) = repo.get("language").and_then(|v| v.as_str()) {
        if !lang.is_empty() {
            topics.push(lang.to_string());
        }
    }
    let starred_at = entry.get("starred_at").cloned().unwrap_or(Value::Null);
    let raw = serde_json::json!({"starred_at": starred_at, "repo": repo});
    let mut item = BackupItem::new(format!("star:{repo_id_str}"), "star", raw).ok()?;
    item.title = repo
        .get("full_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.url = repo
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.body = repo
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.tags = topics;
    item.created_at = entry
        .get("starred_at")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item.updated_at = None;
    Some(item)
}

fn gist_id(gist: &Value) -> Option<String> {
    match gist.get("id")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn gist_item(gist: &Value, gid: &str) -> BackupItem {
    let empty_files = serde_json::Map::new();
    let files = gist
        .get("files")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty_files);
    let mut file_names: Vec<&str> = files.keys().map(String::as_str).collect();
    file_names.sort_unstable();
    let title = gist
        .get("description")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| (!file_names.is_empty()).then(|| file_names.join(", ")))
        .unwrap_or_else(|| gid.to_string());
    let mut tags: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for f in files.values() {
        if let Some(lang) = f.get("language").and_then(|v| v.as_str()) {
            if !lang.is_empty() {
                tags.insert(lang.to_string());
            }
        }
    }
    let mut item = BackupItem::new(format!("gist:{gid}"), "gist", gist.clone())
        .expect("gist: prefix is always non-empty");
    item.title = Some(title);
    item.url = gist
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    item.body = None;
    item.tags = tags.into_iter().collect();
    item.created_at = gist
        .get("created_at")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item.updated_at = gist
        .get("updated_at")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_iso(Some(s)));
    item
}

/// A connector's own `fetch()` reclassifies a non-retryable HTTP
/// status per its own domain knowledge (documented on `HttpError`
/// itself). GitHub returns 401 for a rejected token; 403 means either
/// "rate limit exhausted" (told apart by the `X-RateLimit-Remaining`
/// response header being `"0"`) or "token lacks access" (e.g. a
/// missing `gist` scope) — everything else non-retryable is a
/// transient upstream problem.
fn classify_http_error(e: dbs_core::HttpError) -> ConnectorError {
    match e {
        dbs_core::HttpError::Exhausted(inner) => inner,
        dbs_core::HttpError::Status { error, headers } => {
            match error.status().map(|s| s.as_u16()) {
                Some(401) => ConnectorError::Auth(
                    "GitHub rejected the token (401) — check GITHUB_TOKEN".to_string(),
                ),
                Some(403) => {
                    let exhausted = headers
                        .get("X-RateLimit-Remaining")
                        .and_then(|v| v.to_str().ok())
                        == Some("0");
                    if exhausted {
                        ConnectorError::RateLimited(
                        "GitHub API rate limit exhausted — the next scheduled run resumes from \
                         the last checkpoint"
                            .to_string(),
                    )
                    } else {
                        ConnectorError::Auth(
                            "GitHub returned 403 — the token lacks access (secret gists need the \
                         gist scope)"
                                .to_string(),
                        )
                    }
                }
                Some(status) => ConnectorError::Transient(format!("GitHub API error {status}")),
                None => ConnectorError::Transient(error.to_string()),
            }
        }
        too_large @ dbs_core::HttpError::TooLarge { .. } => {
            ConnectorError::Transient(too_large.to_string())
        }
    }
}

fn bool_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<bool>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ConnectorError::Config(format!("sources.<name>.{key} must be a bool, got {v}"))
    })
}

/// Mirrors the reference's `page_size: int = Field(100, ge=1, le=100)`.
fn page_size_option(
    options: &std::collections::HashMap<String, Value>,
    key: &str,
) -> Result<Option<u32>, ConnectorError> {
    let Some(v) = options.get(key) else {
        return Ok(None);
    };
    let n = v.as_u64().ok_or_else(|| {
        ConnectorError::Config(format!(
            "sources.<name>.{key} must be a positive integer, got {v}"
        ))
    })?;
    if !(1..=100).contains(&n) {
        return Err(ConnectorError::Config(format!(
            "sources.<name>.{key} must be between 1 and 100, got {n}"
        )));
    }
    Ok(Some(n as u32))
}

impl Connector for GitHubConnector {
    fn type_name(&self) -> &str {
        "github"
    }

    fn display_name(&self) -> &str {
        "GitHub"
    }

    fn description(&self) -> &str {
        "Backs up your starred repositories and gists."
    }

    fn docs_url(&self) -> &str {
        "https://docs.github.com/en/rest"
    }

    fn secret_keys(&self) -> &[String] {
        &self.secret_keys
    }

    fn wants_managed_http(&self) -> bool {
        true
    }

    fn volatile_fields(&self) -> &[String] {
        &self.volatile_fields
    }

    fn item_kinds(&self) -> &[ItemKind] {
        &self.item_kinds
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_incremental: true,
            supports_ordered_cursor: true,
            cursor_kind: "timestamp".to_string(),
            supports_full_enumeration: true,
            supports_native_deletes: false,
            produces_media: false,
            requires_auth: true,
            supports_rate_limit_backoff: true,
            paginated: true,
            ..Capabilities::default()
        }
    }

    fn config_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "include_stars": {
                    "type": "boolean",
                    "default": true,
                    "description": "Back up your starred repositories."
                },
                "include_gists": {
                    "type": "boolean",
                    "default": true,
                    "description": "Back up your gists."
                },
                "page_size": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 100,
                    "description": "Items fetched per API page (GitHub's per_page max is 100)."
                }
            }
        })
    }

    fn configure(
        &mut self,
        options: &std::collections::HashMap<String, Value>,
    ) -> Result<(), ConnectorError> {
        if let Some(v) = bool_option(options, "include_stars")? {
            self.config.include_stars = v;
        }
        if let Some(v) = bool_option(options, "include_gists")? {
            self.config.include_gists = v;
        }
        if let Some(v) = page_size_option(options, "page_size")? {
            self.config.page_size = v;
        }
        Ok(())
    }

    fn fetch<'a>(
        &'a mut self,
        ctx: &'a RunContext,
    ) -> Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + 'a> {
        let mut out = Vec::new();

        let Some(http) = ctx.http.as_ref() else {
            out.push(Err(ConnectorError::Config(
                "GitHub connector requires managed HTTP".to_string(),
            )));
            return Box::new(out.into_iter());
        };
        if !self.secret_keys.contains(&self.config.token_env) {
            out.push(Err(ConnectorError::Config(format!(
                "token_env={:?} must be one of the declared secret_keys {:?}",
                self.config.token_env, self.secret_keys
            ))));
            return Box::new(out.into_iter());
        }
        let token = match ctx.secrets.get(&self.config.token_env) {
            Ok(t) => t.to_string(),
            Err(e) => {
                out.push(Err(e));
                return Box::new(out.into_iter());
            }
        };

        let full = ctx.mode == "full" || ctx.mode == "reconcile";
        let mut cursor = ctx
            .cursor
            .as_ref()
            .and_then(|c| c.value.as_object())
            .cloned()
            .unwrap_or_default();
        let mut live_ids = HashSet::new();
        let include_gists = self.config.include_gists;
        let after_stars = move |cursor: &serde_json::Map<String, Value>| {
            if include_gists {
                Stage::Gists(GistsState::new(cursor, full))
            } else {
                Stage::Finish
            }
        };
        let mut stage = if self.config.include_stars {
            Stage::Stars(StarsState::new(&cursor, full))
        } else {
            after_stars(&cursor)
        };
        let this = &*self;

        // One step = one API page. Each page's items and checkpoint reach
        // the host before the next page is requested. An error ends the
        // run: nothing after it is fetched, no phase-done checkpoint
        // advances a watermark, and no reconcile marker is sent.
        step_fetch(
            move |out| match std::mem::replace(&mut stage, Stage::Done) {
                Stage::Stars(mut st) => {
                    match this.stars_page(http, &token, &cursor, &mut st, &mut live_ids, out) {
                        Ok(true) => {
                            stage = Stage::Stars(st);
                            Step::More
                        }
                        Ok(false) => {
                            finish_phase(
                                &mut cursor,
                                "stars_high_watermark",
                                st.max_seen,
                                "stars done",
                                out,
                            );
                            stage = after_stars(&cursor);
                            Step::More
                        }
                        Err(e) => {
                            out.push_back(Err(e));
                            Step::Done
                        }
                    }
                }
                Stage::Gists(mut st) => {
                    match this.gists_page(http, &token, &cursor, &mut st, &mut live_ids, out) {
                        Ok(true) => {
                            stage = Stage::Gists(st);
                            Step::More
                        }
                        Ok(false) => {
                            finish_phase(
                                &mut cursor,
                                "gists_high_watermark",
                                st.max_seen,
                                "gists done",
                                out,
                            );
                            stage = Stage::Finish;
                            Step::More
                        }
                        Err(e) => {
                            out.push_back(Err(e));
                            Step::Done
                        }
                    }
                }
                Stage::Finish => {
                    this.finish(full, std::mem::take(&mut live_ids), out);
                    Step::Done
                }
                Stage::Done => Step::Done,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbs_core::Secrets;
    use std::collections::HashMap;

    fn ctx_with(
        mode: &str,
        cursor: Option<Value>,
        http: ManagedHttpClient,
        token: Option<&str>,
    ) -> RunContext {
        let mut store = HashMap::new();
        if let Some(t) = token {
            store.insert("GITHUB_TOKEN".to_string(), t.to_string());
        }
        RunContext {
            source_id: 1,
            source_name: "github".to_string(),
            cursor: cursor.map(|value| Cursor { value }),
            since: None,
            secrets: Secrets::new(store, vec!["GITHUB_TOKEN".to_string()]),
            run_id: 1,
            mode: mode.to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: Some(RefCell::new(http)),
        }
    }

    fn no_sleep_client() -> ManagedHttpClient {
        ManagedHttpClient::with_sleep(reqwest::blocking::Client::new(), |_| {})
    }

    fn star_json(repo_id: i64, full_name: &str, starred_at: &str) -> Value {
        serde_json::json!({
            "starred_at": starred_at,
            "repo": {
                "id": repo_id,
                "full_name": full_name,
                "html_url": format!("https://github.com/{full_name}"),
                "description": "a repo",
                "topics": ["rust"],
                "language": "Rust",
            }
        })
    }

    fn gist_json(id: &str, description: &str, updated_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "description": description,
            "html_url": format!("https://gist.github.com/{id}"),
            "created_at": updated_at,
            "updated_at": updated_at,
            "files": {"a.rs": {"language": "Rust"}},
        })
    }

    fn events(
        iter: Box<dyn Iterator<Item = Result<FetchEvent, ConnectorError>> + '_>,
    ) -> Vec<FetchEvent> {
        iter.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn fetch_without_managed_http_is_a_config_error() {
        let mut connector = GitHubConnector::new(GitHubConfig::default());
        let ctx = RunContext {
            source_id: 1,
            source_name: "github".to_string(),
            cursor: None,
            since: None,
            secrets: Secrets::new(HashMap::new(), vec!["GITHUB_TOKEN".to_string()]),
            run_id: 1,
            mode: "incremental".to_string(),
            full_refresh: false,
            limit: None,
            store_media: false,
            max_media_bytes: 0,
            download_dir: None,
            items_failed: 0,
            cancel: None,
            http: None,
        };
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Config(_))));
    }

    #[test]
    fn fetch_without_a_token_is_an_auth_error() {
        let server = mockito::Server::new();
        let mut connector =
            GitHubConnector::new(GitHubConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), None);
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0], Err(ConnectorError::Auth(_))));
    }

    #[test]
    fn full_fetch_yields_stars_and_gists_and_a_combined_reconcile_marker() {
        let mut server = mockito::Server::new();
        let stars_page0 = serde_json::json!([star_json(1, "me/repo", "2024-06-01T00:00:00Z")]);
        let empty = serde_json::json!([]);
        let gists_page0 = serde_json::json!([gist_json("g1", "my gist", "2024-06-02T00:00:00Z")]);

        let _m_stars0 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred\?.*page=1.*".to_string()),
            )
            .with_status(200)
            .with_body(stars_page0.to_string())
            .create();
        let _m_stars1 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred\?.*page=2.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();
        let _m_gists0 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/gists\?.*page=1.*".to_string()),
            )
            .with_status(200)
            .with_body(gists_page0.to_string())
            .create();
        let _m_gists1 = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/gists\?.*page=2.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let mut connector =
            GitHubConnector::new(GitHubConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("full", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));

        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 2, "{evs:?}");
        let kinds: HashSet<&str> = items.iter().map(|i| i.item_kind.as_str()).collect();
        assert_eq!(kinds, HashSet::from(["star", "gist"]));

        let marker = evs.iter().find_map(|e| match e {
            FetchEvent::ReconcileMarker(m) => Some(m),
            _ => None,
        });
        assert!(marker.is_some(), "{evs:?}");
        let marker = marker.unwrap();
        assert!(marker.live_ids.contains("star:1") && marker.live_ids.contains("gist:g1"));
    }

    #[test]
    fn reconcile_with_only_stars_enabled_withholds_the_reconcile_marker() {
        let mut server = mockito::Server::new();
        let empty = serde_json::json!([]);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred.*".to_string()),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = GitHubConfig {
            include_gists: false,
            ..Default::default()
        };
        let mut connector = GitHubConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("reconcile", None, no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        assert!(
            !evs.iter()
                .any(|e| matches!(e, FetchEvent::ReconcileMarker(_))),
            "{evs:?}"
        );
    }

    #[test]
    fn incremental_stars_early_stop_past_the_watermark() {
        let mut server = mockito::Server::new();
        let page0 = serde_json::json!([
            star_json(1, "me/new", "2024-06-01T00:00:00Z"),
            star_json(2, "me/old", "2024-01-01T00:00:00Z"),
        ]);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred.*".to_string()),
            )
            .with_status(200)
            .with_body(page0.to_string())
            .create();

        let config = GitHubConfig {
            include_gists: false,
            ..Default::default()
        };
        let mut connector = GitHubConnector::new(config).with_base_url(server.url());
        let cursor = serde_json::json!({"stars_high_watermark": "2024-03-01T00:00:00Z"});
        let ctx = ctx_with("incremental", Some(cursor), no_sleep_client(), Some("tok"));
        let evs = events(connector.fetch(&ctx));
        let items: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                FetchEvent::Item(i) => Some(i),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id(), "star:1");
    }

    /// Page N+1 must not be requested until page N's events, ending in
    /// its checkpoint, have been handed to the caller.
    #[test]
    fn fetch_yields_each_page_before_requesting_the_next() {
        let mut server = mockito::Server::new();
        let page_query =
            |page: &str| mockito::Matcher::UrlEncoded("page".to_string(), page.to_string());
        let m0 = server
            .mock("GET", "/user/starred")
            .match_query(page_query("1"))
            .with_status(200)
            .with_body(
                serde_json::json!([star_json(1, "me/a", "2024-06-02T00:00:00Z")]).to_string(),
            )
            .expect(1)
            .create();
        let m1 = server
            .mock("GET", "/user/starred")
            .match_query(page_query("2"))
            .with_status(200)
            .with_body(serde_json::json!([]).to_string())
            .expect(1)
            .create();

        let config = GitHubConfig {
            include_gists: false,
            page_size: 1,
            ..Default::default()
        };
        let mut connector = GitHubConnector::new(config).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), Some("tok"));
        let mut iter = connector.fetch(&ctx);

        // Page 1: one star, then its checkpoint.
        assert!(matches!(iter.next(), Some(Ok(FetchEvent::Item(_)))));
        match iter.next() {
            Some(Ok(FetchEvent::Checkpoint(cp))) => assert_eq!(cp.note, "stars page 1"),
            other => panic!("expected page 1's checkpoint, got {other:?}"),
        }
        m0.assert();
        assert!(
            !m1.matched(),
            "page 2 was requested before page 1 was consumed"
        );

        // Draining fetches page 2 (empty) exactly once, then the phase
        // ends with its watermark-advancing checkpoint.
        let rest: Vec<FetchEvent> = iter.map(|r| r.unwrap()).collect();
        m1.assert();
        match rest.as_slice() {
            [FetchEvent::Checkpoint(cp)] => {
                assert_eq!(cp.note, "stars done");
                assert_eq!(
                    cp.cursor.value["stars_high_watermark"],
                    "2024-06-02T00:00:00Z"
                );
            }
            other => panic!("expected only the stars-done checkpoint, got {other:?}"),
        }
    }

    #[test]
    fn incremental_gists_sends_the_stored_watermark_as_a_since_param() {
        let mut server = mockito::Server::new();
        let empty = serde_json::json!([]);
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::AllOf(vec![
                    mockito::Matcher::Regex(r"^/gists\?".to_string()),
                    mockito::Matcher::Regex(r"since=2024-05-01T00%3A00%3A00Z".to_string()),
                ]),
            )
            .with_status(200)
            .with_body(empty.to_string())
            .create();

        let config = GitHubConfig {
            include_stars: false,
            ..Default::default()
        };
        let mut connector = GitHubConnector::new(config).with_base_url(server.url());
        let cursor = serde_json::json!({"gists_high_watermark": "2024-05-01T00:00:00Z"});
        let ctx = ctx_with("incremental", Some(cursor), no_sleep_client(), Some("tok"));
        let _ = events(connector.fetch(&ctx));
        _m.assert();
    }

    #[test]
    fn a_401_response_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred.*".to_string()),
            )
            .with_status(401)
            .with_body("{}")
            .create();

        let mut connector =
            GitHubConnector::new(GitHubConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), Some("bad"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn a_403_with_rate_limit_remaining_zero_is_classified_as_rate_limited() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred.*".to_string()),
            )
            .with_status(403)
            .with_header("X-RateLimit-Remaining", "0")
            .with_body("{}")
            .create();

        let mut connector =
            GitHubConnector::new(GitHubConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), Some("tok"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::RateLimited(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn a_403_without_rate_limit_header_is_classified_as_an_auth_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock(
                "GET",
                mockito::Matcher::Regex(r"^/user/starred.*".to_string()),
            )
            .with_status(403)
            .with_body("{}")
            .create();

        let mut connector =
            GitHubConnector::new(GitHubConfig::default()).with_base_url(server.url());
        let ctx = ctx_with("incremental", None, no_sleep_client(), Some("tok"));
        let result: Vec<_> = connector.fetch(&ctx).collect();
        assert!(
            result
                .iter()
                .any(|r| matches!(r, Err(ConnectorError::Auth(_)))),
            "{result:?}"
        );
    }

    #[test]
    fn connector_metadata_matches_the_reference() {
        let connector = GitHubConnector::new(GitHubConfig::default());
        assert_eq!(connector.type_name(), "github");
        assert_eq!(connector.secret_keys(), &["GITHUB_TOKEN".to_string()]);
        assert!(connector.wants_managed_http());
        assert_eq!(connector.item_kinds().len(), 2);
        assert!(connector.capabilities().requires_auth);
        assert!(!connector.capabilities().supports_native_deletes);
        assert_eq!(connector.volatile_fields(), &["repo".to_string()]);
    }

    #[test]
    fn config_schema_describes_every_key_configure_reads() {
        let connector = GitHubConnector::new(GitHubConfig::default());
        let schema = connector.config_schema();
        let props = schema["properties"].as_object().unwrap();
        for key in ["include_stars", "include_gists", "page_size"] {
            assert!(props.contains_key(key), "missing schema key: {key}");
        }
        assert_eq!(schema["properties"]["page_size"]["maximum"], 100);
    }

    #[test]
    fn configure_applies_include_stars_include_gists_and_page_size_from_options() {
        let mut connector = GitHubConnector::new(GitHubConfig::default());
        let options = HashMap::from([
            ("include_stars".to_string(), serde_json::json!(false)),
            ("include_gists".to_string(), serde_json::json!(false)),
            ("page_size".to_string(), serde_json::json!(25)),
        ]);
        connector.configure(&options).unwrap();
        assert!(!connector.config.include_stars);
        assert!(!connector.config.include_gists);
        assert_eq!(connector.config.page_size, 25);
    }

    #[test]
    fn configure_with_no_matching_keys_leaves_defaults_untouched() {
        let mut connector = GitHubConnector::new(GitHubConfig::default());
        connector.configure(&HashMap::new()).unwrap();
        assert!(connector.config.include_stars);
        assert!(connector.config.include_gists);
        assert_eq!(connector.config.page_size, 100);
    }

    #[test]
    fn configure_rejects_a_non_bool_include_stars() {
        let mut connector = GitHubConnector::new(GitHubConfig::default());
        let options = HashMap::from([("include_stars".to_string(), serde_json::json!("yes"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_page_size_outside_1_to_100() {
        let mut connector = GitHubConnector::new(GitHubConfig::default());
        let options = HashMap::from([("page_size".to_string(), serde_json::json!(101))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }

    #[test]
    fn configure_rejects_a_non_integer_page_size() {
        let mut connector = GitHubConnector::new(GitHubConfig::default());
        let options = HashMap::from([("page_size".to_string(), serde_json::json!("big"))]);
        let err = connector.configure(&options).unwrap_err();
        assert!(matches!(err, ConnectorError::Config(_)));
    }
}
