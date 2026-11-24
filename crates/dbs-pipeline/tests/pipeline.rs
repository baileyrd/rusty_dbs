//! End-to-end batches against fake HTTP servers: each fallback branch,
//! rate limiting, resume after a crash, dedupe and cancellation.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::NaiveDate;
use dbs_core::{CancelToken, ManagedHttpClient};
use dbs_pipeline::fetch::CopyStore;
use dbs_pipeline::ledger::{Entry, Ledger, Status};
use dbs_pipeline::runner::{run, Batch, Summary};
use dbs_pipeline::source::{Bookmark, BookmarkSource, RaindropApi};
use mockito::{Matcher, Server, ServerGuard};
use url::Url;

const ARTICLE: &str = "<html><head><title>Cron returns</title></head><body><article>\
<p>Teams are moving scheduled jobs back to cron, because a cron line is easy to read, cheap, and hard to break.</p>\
<p>The notes list the jobs that moved, with costs, incidents, and timings, for each team.</p></article></body></html>";

/// A copy store with no copies, for tests that do not reach Raindrop.
struct NoCopies;

impl CopyStore for NoCopies {
    fn copy_location(&self, _id: u64) -> Result<Option<Url>, String> {
        Ok(None)
    }
}

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("dbs-pipeline-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir");
        Dir(path)
    }

    fn ledger_path(&self) -> PathBuf {
        self.0.join("ledger.jsonl")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn no_sleep_client(redirects: reqwest::redirect::Policy) -> ManagedHttpClient {
    let client = reqwest::blocking::Client::builder()
        .redirect(redirects)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client");
    ManagedHttpClient::with_sleep(client, |_| {}).max_attempts(2)
}

fn pages() -> Result<ManagedHttpClient, String> {
    Ok(no_sleep_client(reqwest::redirect::Policy::limited(10)))
}

fn raindrop(server: &ServerGuard) -> RaindropApi {
    RaindropApi::new(
        no_sleep_client(reqwest::redirect::Policy::none()),
        Url::parse(&server.url()).expect("url"),
        "secret-token".to_string(),
    )
}

fn bookmark(id: u64, link: &str) -> Bookmark {
    Bookmark {
        id,
        link: Url::parse(link).expect("link"),
        title: "Saved title".to_string(),
        tags: vec!["ops".to_string()],
        saved: NaiveDate::from_ymd_opt(2026, 11, 1),
        highlights: vec![],
    }
}

fn batch(dir: &Dir, wayback: &str, concurrency: usize) -> Batch {
    Batch {
        out_dir: dir.0.join("notes"),
        clipped: NaiveDate::from_ymd_opt(2026, 11, 24).expect("date"),
        concurrency,
        wayback_base: Url::parse(wayback).expect("wayback url"),
    }
}

fn run_with<C: CopyStore + Sync + ?Sized>(
    dir: &Dir,
    bookmarks: Vec<Bookmark>,
    copies: &C,
    wayback: &str,
    cancel: &CancelToken,
) -> Summary {
    let (ledger, _) = Ledger::open(&dir.ledger_path()).expect("ledger");
    run(
        bookmarks,
        &Mutex::new(ledger),
        copies,
        &pages,
        &batch(dir, wayback, 1),
        cancel,
    )
    .expect("run")
}

fn entries(path: &Path) -> Vec<Entry> {
    fs::read_to_string(path)
        .expect("ledger file")
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[test]
fn live_page_is_clipped_and_recorded() {
    let mut site = Server::new();
    let page = site
        .mock("GET", "/post")
        .with_header("content-type", "text/html")
        .with_body(ARTICLE)
        .create();
    let dir = Dir::new("live");
    let summary = run_with(
        &dir,
        vec![bookmark(11, &format!("{}/post", site.url()))],
        &NoCopies,
        "http://unused.invalid/",
        &CancelToken::new(),
    );
    page.assert();
    assert_eq!(summary.done, 1);
    let note = fs::read_to_string(dir.0.join("notes/11.md")).expect("note");
    assert!(note.contains("raindrop_id: 11\n"), "{note}");
    assert!(note.contains("fetch_path: live\n"));
    assert!(note.contains("cron line is easy to read"));
    let e = &entries(&dir.ledger_path())[0];
    assert_eq!(
        (e.id, e.status, e.fetch_path.map(|p| p.as_str())),
        (11, Status::Done, Some("live"))
    );
    assert!(e.content_hash.as_deref().is_some_and(|h| note.contains(h)));
}

#[test]
fn dead_page_falls_back_to_raindrop_copy_without_leaking_the_token() {
    let mut site = Server::new();
    site.mock("GET", "/gone").with_status(404).create();
    let mut api_server = Server::new();
    let mut storage = Server::new();
    let cache = api_server
        .mock("GET", "/rest/v1/raindrop/22/cache")
        .match_header("authorization", "Bearer secret-token")
        .with_status(307)
        .with_header("location", &format!("{}/copies/22", storage.url()))
        .create();
    let copy = storage
        .mock("GET", "/copies/22")
        .match_header("authorization", Matcher::Missing)
        .with_header("content-type", "text/plain")
        .with_body("Raindrop kept this text, with commas, and periods, for twenty years.\n\nA second paragraph, also long enough, to count as content.")
        .create();
    let dir = Dir::new("cache");
    let api = Mutex::new(raindrop(&api_server));
    let summary = run_with(
        &dir,
        vec![bookmark(22, &format!("{}/gone", site.url()))],
        &api,
        "http://unused.invalid/",
        &CancelToken::new(),
    );
    cache.assert();
    copy.assert();
    assert_eq!(summary.done, 1);
    let e = &entries(&dir.ledger_path())[0];
    assert_eq!(e.fetch_path.map(|p| p.as_str()), Some("raindrop_cache"));
    let note = fs::read_to_string(dir.0.join("notes/22.md")).expect("note");
    assert!(note.contains("Raindrop kept this text"));
    assert!(note.contains(&format!("source: \"{}/gone\"", site.url())));
}

#[test]
fn falls_back_to_wayback_raw_snapshot() {
    let mut site = Server::new();
    let live = site.mock("GET", "/old").with_status(503).expect(2).create();
    let mut wayback = Server::new();
    let original = format!("{}/old", site.url());
    let snapshot = format!("{}/web/20150701000000/{original}", wayback.url());
    wayback
        .mock("GET", "/wayback/available")
        .match_query(Matcher::UrlEncoded("url".into(), original.clone()))
        .with_header("content-type", "application/json")
        .with_body(format!(r#"{{"archived_snapshots":{{"closest":{{"available":true,"status":"200","url":"{snapshot}"}}}}}}"#))
        .create();
    let raw = wayback
        .mock("GET", format!("/web/20150701000000id_/{original}").as_str())
        .with_header("content-type", "text/html")
        .with_body(ARTICLE)
        .create();
    let dir = Dir::new("wayback");
    let summary = run_with(
        &dir,
        vec![bookmark(33, &original)],
        &NoCopies,
        &format!("{}/", wayback.url()),
        &CancelToken::new(),
    );
    live.assert();
    raw.assert();
    assert_eq!(summary.done, 1);
    let e = &entries(&dir.ledger_path())[0];
    assert_eq!(e.fetch_path.map(|p| p.as_str()), Some("wayback"));
}

#[test]
fn every_path_failing_records_the_whole_chain_and_is_retried_later() {
    let mut site = Server::new();
    site.mock("GET", "/x").with_status(410).create();
    let mut wayback = Server::new();
    wayback
        .mock("GET", "/wayback/available")
        .match_query(Matcher::Any)
        .with_body(r#"{"archived_snapshots":{}}"#)
        .create();
    let dir = Dir::new("fail");
    let link = format!("{}/x", site.url());
    let summary = run_with(
        &dir,
        vec![bookmark(44, &link)],
        &NoCopies,
        &format!("{}/", wayback.url()),
        &CancelToken::new(),
    );
    assert_eq!(summary.failed, 1);
    let e = &entries(&dir.ledger_path())[0];
    let reason = e.reason.as_deref().unwrap_or_default();
    assert!(
        reason.starts_with("live: ") && reason.contains("410"),
        "{reason}"
    );
    assert!(
        reason.contains("raindrop_cache: no stored copy"),
        "{reason}"
    );
    assert!(reason.contains("wayback: no Wayback snapshot"), "{reason}");
    let (ledger, _) = Ledger::read_only(&dir.ledger_path()).expect("ledger");
    assert!(!ledger.is_settled(44), "failed ids are retried");
}

#[test]
fn pdf_is_recorded_as_skipped() {
    let mut site = Server::new();
    site.mock("GET", "/paper.pdf")
        .with_header("content-type", "application/pdf")
        .with_body("%PDF-1.7 ...")
        .create();
    let dir = Dir::new("pdf");
    let summary = run_with(
        &dir,
        vec![bookmark(55, &format!("{}/paper.pdf", site.url()))],
        &NoCopies,
        "http://unused.invalid/",
        &CancelToken::new(),
    );
    assert_eq!(summary.skipped, 1);
    let e = &entries(&dir.ledger_path())[0];
    assert_eq!(
        (e.status, e.reason.as_deref()),
        (Status::Skipped, Some("pdf"))
    );
    assert!(!dir.0.join("notes/55.md").exists());
}

#[test]
fn same_article_saved_twice_is_deduped_by_url_and_by_content() {
    let mut site = Server::new();
    let page = site
        .mock("GET", "/post")
        .match_query(Matcher::Any)
        .with_header("content-type", "text/html")
        .with_body(ARTICLE)
        .expect(1)
        .create();
    site.mock("GET", "/mirror")
        .with_header("content-type", "text/html")
        .with_body(ARTICLE)
        .create();
    let dir = Dir::new("dedupe");
    let summary = run_with(
        &dir,
        vec![
            bookmark(1902093857, &format!("{}/post", site.url())),
            bookmark(1900852003, &format!("{}/post?utm_source=feed", site.url())),
            bookmark(1900000001, &format!("{}/mirror", site.url())),
        ],
        &NoCopies,
        "http://unused.invalid/",
        &CancelToken::new(),
    );
    page.assert();
    assert_eq!((summary.done, summary.skipped), (1, 2));
    let e = entries(&dir.ledger_path());
    assert_eq!(e[1].duplicate_of, Some(1902093857));
    assert_eq!(e[1].reason.as_deref(), Some("duplicate: same URL"));
    assert_eq!(e[2].duplicate_of, Some(1902093857));
    assert_eq!(e[2].reason.as_deref(), Some("duplicate: same content"));
}

#[test]
fn resume_after_a_crash_skips_done_ids_and_survives_a_torn_line() {
    let mut site = Server::new();
    let done_page = site.mock("GET", "/done").expect(0).create();
    let next_page = site
        .mock("GET", "/next")
        .with_header("content-type", "text/html")
        .with_body(ARTICLE)
        .create();
    let dir = Dir::new("resume");
    let done_line = format!(
        r#"{{"id":1,"status":"done","url":"{}/done","fetch_path":"live","content_hash":"aa","at":"2026-11-24T08:00:00Z"}}"#,
        site.url()
    );
    // A crash mid-write leaves a partial last line.
    fs::write(dir.ledger_path(), format!("{done_line}\n{{\"id\":2,\"sta")).expect("seed ledger");
    let (ledger, report) = Ledger::open(&dir.ledger_path()).expect("ledger");
    assert_eq!((report.entries, report.unreadable), (1, 1));
    let bookmarks = vec![
        bookmark(1, &format!("{}/done", site.url())),
        bookmark(2, &format!("{}/next", site.url())),
    ];
    let summary = run(
        bookmarks,
        &Mutex::new(ledger),
        &NoCopies,
        &pages,
        &batch(&dir, "http://unused.invalid/", 1),
        &CancelToken::new(),
    )
    .expect("run");
    done_page.assert();
    next_page.assert();
    assert_eq!((summary.already_settled, summary.done), (1, 1));
    let (_, report) = Ledger::read_only(&dir.ledger_path()).expect("reload");
    assert_eq!((report.entries, report.unreadable), (2, 1));
    let last = entries(&dir.ledger_path()).pop().expect("entry");
    assert_eq!((last.id, last.status), (2, Status::Done));
}

#[test]
fn cancelled_run_starts_nothing_and_writes_nothing() {
    let dir = Dir::new("cancel");
    let cancel = CancelToken::new();
    cancel.cancel();
    let summary = run_with(
        &dir,
        vec![
            bookmark(1, "https://a.invalid/"),
            bookmark(2, "https://b.invalid/"),
        ],
        &NoCopies,
        "http://unused.invalid/",
        &cancel,
    );
    assert_eq!(summary.not_started, 2);
    assert!(entries(&dir.ledger_path()).is_empty());
}

#[test]
fn concurrent_workers_settle_every_item_once() {
    let mut site = Server::new();
    let page = site
        .mock("GET", Matcher::Regex("^/p/\\d+$".into()))
        .with_header("content-type", "text/html")
        .with_body(ARTICLE)
        .expect(1)
        .create();
    let other = site
        .mock("GET", Matcher::Regex("^/q/\\d+$".into()))
        .with_status(404)
        .expect(5)
        .create();
    let dir = Dir::new("concurrent");
    let mut bookmarks = vec![bookmark(100, &format!("{}/p/0", site.url()))];
    bookmarks.extend((1..=5).map(|i| bookmark(100 + i, &format!("{}/q/{i}", site.url()))));
    let (ledger, _) = Ledger::open(&dir.ledger_path()).expect("ledger");
    let summary = run(
        bookmarks,
        &Mutex::new(ledger),
        &NoCopies,
        &pages,
        &batch(&dir, "http://127.0.0.1:9/", 4),
        &CancelToken::new(),
    )
    .expect("run");
    page.assert();
    other.assert();
    assert_eq!((summary.done, summary.failed), (1, 5));
    let mut ids: Vec<u64> = entries(&dir.ledger_path()).iter().map(|e| e.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, (100..=105).collect::<Vec<_>>());
}

#[test]
fn source_honours_retry_after_and_pages_newest_first() {
    let mut api_server = Server::new();
    let limited = api_server
        .mock("GET", "/rest/v1/raindrops/0")
        .match_query(Matcher::UrlEncoded("page".into(), "0".into()))
        .with_status(429)
        .with_header("retry-after", "1")
        .expect(1)
        .create();
    let items: Vec<String> = (0..50)
        .map(|i| {
            format!(
                r#"{{"_id":{},"link":"https://e.com/{i}","created":"2026-11-0{}T00:00:00Z"}}"#,
                1000 - i,
                1 + i % 9
            )
        })
        .collect();
    let page0 = api_server
        .mock("GET", "/rest/v1/raindrops/0")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("page".into(), "0".into()),
            Matcher::UrlEncoded("sort".into(), "-created".into()),
        ]))
        .match_header("authorization", "Bearer secret-token")
        .with_body(format!(r#"{{"items":[{}]}}"#, items.join(",")))
        .create();
    let page1 = api_server
        .mock("GET", "/rest/v1/raindrops/0")
        .match_query(Matcher::UrlEncoded("page".into(), "1".into()))
        .with_body(
            r#"{"items":[{"_id":900,"link":"https://e.com/last"},{"_id":899,"link":"ftp://bad"}]}"#,
        )
        .create();
    let listing = raindrop(&api_server).newest(60).expect("listing");
    limited.assert();
    page0.assert();
    page1.assert();
    assert_eq!(listing.bookmarks.len(), 51);
    assert_eq!(listing.bookmarks[0].id, 1000);
    assert_eq!(listing.rejected.len(), 1);
    assert_eq!(listing.rejected[0].id, Some(899));
}

#[test]
fn by_ids_keeps_order_and_reports_auth_failures() {
    let mut api_server = Server::new();
    api_server
        .mock("GET", "/rest/v1/raindrop/7")
        .with_body(r#"{"item":{"_id":7,"link":"https://e.com/7","highlights":[{"text":"hi"}]}}"#)
        .create();
    api_server
        .mock("GET", "/rest/v1/raindrop/8")
        .with_status(404)
        .create();
    let listing = raindrop(&api_server).by_ids(&[8, 7]).expect("listing");
    assert_eq!(listing.bookmarks[0].highlights, ["hi"]);
    assert_eq!(listing.rejected[0].id, Some(8));

    let mut denied = Server::new();
    denied
        .mock("GET", "/rest/v1/raindrop/7")
        .with_status(401)
        .create();
    let err = raindrop(&denied).by_ids(&[7]).expect_err("auth");
    assert!(
        err.to_string().starts_with("authentication failed"),
        "{err}"
    );
    assert!(!err.to_string().contains("secret-token"));
}
