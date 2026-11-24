//! The batch runner: fetch, clip and write each bookmark, recording every
//! outcome in the ledger.
//!
//! Workers share one ledger (behind a mutex, held only to claim or record,
//! never during a fetch) and one Raindrop client (one rate limiter). A
//! cancelled run stops taking items; items in flight finish and record,
//! so the ledger stays consistent.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{NaiveDate, Utc};
use dbs_clip::{clip, ClipError, FetchPath, SourceMeta};
use dbs_core::{CancelToken, ManagedHttpClient};
use url::Url;

use crate::fetch::{Content, CopyStore, Fetched, Fetcher};
use crate::ledger::{Entry, Ledger, Status};
use crate::source::Bookmark;

/// Settings for one batch.
#[derive(Debug, Clone)]
pub struct Batch {
    /// Notes are written here as `<raindrop id>.md`.
    pub out_dir: PathBuf,
    /// The `clipped:` date for every note in the batch.
    pub clipped: NaiveDate,
    /// Bookmarks fetched at once (at least one).
    pub concurrency: usize,
    /// Where Wayback lives: `https://archive.org/` outside tests.
    pub wayback_base: Url,
}

/// Counts for one run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub done: usize,
    pub failed: usize,
    pub skipped: usize,
    /// Settled by an earlier run, so not touched.
    pub already_settled: usize,
    /// Not started because the run was cancelled.
    pub not_started: usize,
}

enum Outcome {
    Recorded(Status),
    AlreadySettled,
}

/// Runs the batch. `pages` makes one page client per worker.
///
/// # Errors
/// A ledger or note write failure stops the run: without the ledger the
/// batch could not resume safely.
pub fn run<C>(
    bookmarks: Vec<Bookmark>,
    ledger: &Mutex<Ledger>,
    copies: &C,
    pages: &(dyn Fn() -> Result<ManagedHttpClient, String> + Sync),
    batch: &Batch,
    cancel: &CancelToken,
) -> io::Result<Summary>
where
    C: CopyStore + Sync + ?Sized,
{
    fs::create_dir_all(&batch.out_dir)?;
    let queue = Mutex::new(VecDeque::from(bookmarks));
    let summary = Mutex::new(Summary::default());
    let workers = batch.concurrency.max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| scope.spawn(|| worker(&queue, ledger, copies, pages, batch, cancel, &summary)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(io::Error::other("worker panicked")))
            })
            .collect::<io::Result<Vec<()>>>()
    })?;
    let mut summary = lock(&summary)?.to_owned();
    summary.not_started = lock(&queue)?.len();
    Ok(summary)
}

fn worker<C: CopyStore + Sync + ?Sized>(
    queue: &Mutex<VecDeque<Bookmark>>,
    ledger: &Mutex<Ledger>,
    copies: &C,
    pages: &(dyn Fn() -> Result<ManagedHttpClient, String> + Sync),
    batch: &Batch,
    cancel: &CancelToken,
    summary: &Mutex<Summary>,
) -> io::Result<()> {
    let client = pages().map_err(io::Error::other)?;
    let mut fetcher = Fetcher::new(client, copies, batch.wayback_base.clone());
    loop {
        if cancel.cancelled() {
            return Ok(());
        }
        let Some(bookmark) = lock(queue)?.pop_front() else {
            return Ok(());
        };
        let outcome = process(&bookmark, &mut fetcher, ledger, batch)?;
        count(&mut *lock(summary)?, outcome);
    }
}

fn count(summary: &mut Summary, outcome: Outcome) {
    match outcome {
        Outcome::AlreadySettled => summary.already_settled += 1,
        Outcome::Recorded(Status::Done) => summary.done += 1,
        Outcome::Recorded(Status::Failed) => summary.failed += 1,
        Outcome::Recorded(Status::Skipped) => summary.skipped += 1,
    }
}

fn process<C: CopyStore + ?Sized>(
    bookmark: &Bookmark,
    fetcher: &mut Fetcher<'_, C>,
    ledger: &Mutex<Ledger>,
    batch: &Batch,
) -> io::Result<Outcome> {
    {
        let mut ledger = lock(ledger)?;
        if ledger.is_settled(bookmark.id) {
            return Ok(Outcome::AlreadySettled);
        }
        if let Some(owner) = ledger.claim_url(bookmark.id, &bookmark.link) {
            let entry = entry(bookmark, Status::Skipped).duplicate(owner, "same URL");
            return record(&mut ledger, entry);
        }
    }
    let entry = match fetcher.fetch(bookmark) {
        Err(failure) => entry(bookmark, Status::Failed).reason(failure.to_string()),
        Ok(fetched) => return settle_fetched(bookmark, fetched, ledger, batch),
    };
    record(&mut *lock(ledger)?, entry)
}

fn settle_fetched(
    bookmark: &Bookmark,
    fetched: Fetched,
    ledger: &Mutex<Ledger>,
    batch: &Batch,
) -> io::Result<Outcome> {
    let base = entry(bookmark, Status::Skipped).path(fetched.path);
    let html = match fetched.content {
        Content::Pdf => return record(&mut *lock(ledger)?, base.reason("pdf".to_string())),
        Content::Html(html) => html,
        Content::Text(text) => text_to_html(&text),
    };
    let meta = source_meta(bookmark, &fetched.url, fetched.path, batch.clipped);
    let clipped = match clip(&html, &meta) {
        Ok(c) => c,
        Err(ClipError::NoContent) => {
            let failed = Entry {
                status: Status::Failed,
                ..base
            };
            let reason = format!("{} page had no article content", fetched.path);
            return record(&mut *lock(ledger)?, failed.reason(reason));
        }
    };
    let mut ledger = lock(ledger)?;
    if let Some(owner) = ledger.hash_owner(bookmark.id, &clipped.content_hash) {
        let entry = base
            .duplicate(owner, "same content")
            .hash(clipped.content_hash);
        return record(&mut ledger, entry);
    }
    write_note(&batch.out_dir, bookmark.id, &clipped.markdown())?;
    let done = Entry {
        status: Status::Done,
        ..base
    }
    .hash(clipped.content_hash);
    record(&mut ledger, done)
}

/// The note's `source:` is the bookmark, except after a live redirect
/// (share.google), where the page actually read is the useful link.
fn source_meta(
    bookmark: &Bookmark,
    fetched_url: &Url,
    path: FetchPath,
    clipped: NaiveDate,
) -> SourceMeta {
    let source = if path == FetchPath::Live {
        fetched_url
    } else {
        &bookmark.link
    };
    SourceMeta {
        source: source.clone(),
        clipped,
        fetch_path: path,
        raindrop_id: Some(bookmark.id),
        saved: bookmark.saved,
        title: Some(bookmark.title.clone()).filter(|t| !t.is_empty()),
        tags: bookmark.tags.clone(),
        highlights: bookmark.highlights.clone(),
    }
}

fn record(ledger: &mut Ledger, entry: Entry) -> io::Result<Outcome> {
    let status = entry.status;
    ledger.record(&entry)?;
    Ok(Outcome::Recorded(status))
}

fn entry(bookmark: &Bookmark, status: Status) -> Entry {
    Entry {
        id: bookmark.id,
        status,
        url: bookmark.link.to_string(),
        fetch_path: None,
        content_hash: None,
        at: Utc::now(),
        reason: None,
        duplicate_of: None,
    }
}

impl Entry {
    fn reason(self, reason: String) -> Self {
        Entry {
            reason: Some(reason),
            ..self
        }
    }

    fn path(self, path: FetchPath) -> Self {
        Entry {
            fetch_path: Some(path),
            ..self
        }
    }

    fn hash(self, hash: String) -> Self {
        Entry {
            content_hash: Some(hash),
            ..self
        }
    }

    fn duplicate(self, owner: u64, why: &str) -> Self {
        Entry {
            duplicate_of: Some(owner),
            reason: Some(format!("duplicate: {why}")),
            ..self
        }
    }
}

/// Writes `<dir>/<id>.md` through a temporary file and a rename, so a
/// crash never leaves a half-written note.
pub fn write_note(dir: &Path, id: u64, markdown: &str) -> io::Result<()> {
    let tmp = dir.join(format!("{id}.md.tmp"));
    fs::write(&tmp, markdown)?;
    fs::rename(&tmp, dir.join(format!("{id}.md")))
}

/// Raindrop's stored copy can be flat text: blank lines become
/// paragraphs and line breaks stay, so the clip core can read it.
pub fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let paragraphs: Vec<String> = escaped
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", p.replace('\n', "<br>")))
        .collect();
    format!(
        "<html><body><article>{}</article></body></html>",
        paragraphs.join("\n")
    )
}

fn lock<T>(m: &Mutex<T>) -> io::Result<std::sync::MutexGuard<'_, T>> {
    m.lock()
        .map_err(|_| io::Error::other("a worker panicked while holding a lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_to_html_escapes_and_splits_paragraphs() {
        assert_eq!(
            text_to_html("a < b & c\nnext\n\n\nsecond"),
            "<html><body><article><p>a &lt; b &amp; c<br>next</p>\n<p>second</p></article></body></html>"
        );
    }

    #[test]
    fn write_note_replaces_atomically() {
        let dir = std::env::temp_dir().join(format!("dbs-pipeline-note-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        write_note(&dir, 7, "one").expect("write");
        write_note(&dir, 7, "two").expect("rewrite");
        assert_eq!(fs::read_to_string(dir.join("7.md")).expect("read"), "two");
        assert!(!dir.join("7.md.tmp").exists());
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
