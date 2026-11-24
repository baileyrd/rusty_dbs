//! `dbs-pipeline`: run (or dry-run) one knowledge-pipeline batch.
//!
//! ```text
//! dbs-pipeline --out DIR (--ids 1,2,3 | --newest N) [--dry-run]
//!              [--ledger PATH] [--concurrency 4] [--token-env RAINDROP_TOKEN]
//! ```
//!
//! The Raindrop token is read from the environment and never printed.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbs_core::CancelToken;
use dbs_pipeline::fetch::page_http_client;
use dbs_pipeline::ledger::Ledger;
use dbs_pipeline::plan::plan;
use dbs_pipeline::runner::{run, Batch};
use dbs_pipeline::source::{api_http_client, BookmarkSource, Listing, RaindropApi};
use url::Url;

const RAINDROP_BASE: &str = "https://api.raindrop.io/";
const WAYBACK_BASE: &str = "https://archive.org/";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Selection {
    Ids(Vec<u64>),
    Newest(usize),
}

#[derive(Debug)]
struct Args {
    out: PathBuf,
    ledger: PathBuf,
    selection: Selection,
    dry_run: bool,
    concurrency: usize,
    token_env: String,
}

const USAGE: &str = "usage: dbs-pipeline --out DIR (--ids 1,2,3 | --newest N) [--dry-run] \
[--ledger PATH] [--concurrency N] [--token-env NAME]";

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match execute(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn execute(args: &Args) -> Result<(), String> {
    let token = std::env::var(&args.token_env)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| format!("set {} to a Raindrop API token", args.token_env))?;
    let base = Url::parse(RAINDROP_BASE).map_err(|e| e.to_string())?;
    let mut api = RaindropApi::new(
        api_http_client(HTTP_TIMEOUT).map_err(|e| e.to_string())?,
        base,
        token,
    );
    let listing = match &args.selection {
        Selection::Ids(ids) => api.by_ids(ids),
        Selection::Newest(n) => api.newest(*n),
    }
    .map_err(|e| e.to_string())?;
    if args.dry_run {
        return dry_run(args, &listing);
    }
    run_batch(args, api, listing)
}

fn dry_run(args: &Args, listing: &Listing) -> Result<(), String> {
    let (mut ledger, report) =
        Ledger::read_only(&args.ledger).map_err(|e| format!("ledger: {e}"))?;
    eprintln!(
        "ledger: {} entries, {} unreadable",
        report.entries, report.unreadable
    );
    for line in plan(&mut ledger, listing) {
        println!("{line}");
    }
    Ok(())
}

fn run_batch(args: &Args, api: RaindropApi, listing: Listing) -> Result<(), String> {
    std::fs::create_dir_all(&args.out).map_err(|e| format!("{}: {e}", args.out.display()))?;
    let (ledger, report) = Ledger::open(&args.ledger).map_err(|e| format!("ledger: {e}"))?;
    if report.unreadable > 0 {
        eprintln!(
            "ledger: skipped {} unreadable line(s), likely a torn write",
            report.unreadable
        );
    }
    for rejected in &listing.rejected {
        eprintln!("rejected {:?}: {}", rejected.id, rejected.reason);
    }
    let batch = Batch {
        out_dir: args.out.clone(),
        clipped: chrono::Utc::now().date_naive(),
        concurrency: args.concurrency,
        wayback_base: Url::parse(WAYBACK_BASE).map_err(|e| e.to_string())?,
    };
    let cancel = install_stop_handler();
    let pages = || page_http_client(HTTP_TIMEOUT);
    let summary = run(
        listing.bookmarks,
        &Mutex::new(ledger),
        &Mutex::new(api),
        &pages,
        &batch,
        &cancel,
    )
    .map_err(|e| e.to_string())?;
    println!(
        "done {}, failed {}, skipped {}, settled earlier {}, not started {}",
        summary.done, summary.failed, summary.skipped, summary.already_settled, summary.not_started
    );
    Ok(())
}

/// First Ctrl-C: stop taking items, let in-flight ones finish. Second:
/// exit at once (each ledger line is already synced, so it stays valid).
fn install_stop_handler() -> CancelToken {
    let cancel = CancelToken::new();
    let handler_cancel = cancel.clone();
    let hits = Arc::new(AtomicUsize::new(0));
    let installed = ctrlc::set_handler(move || {
        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
            handler_cancel.cancel();
            eprintln!("\nStopping after the items in flight (Ctrl-C again to abort now).");
            return;
        }
        eprintln!("\nAborted.");
        std::process::exit(130);
    });
    if installed.is_err() {
        eprintln!("warning: could not install a Ctrl-C handler");
    }
    cancel
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut out = None;
    let mut ledger = None;
    let mut selection = None;
    let mut dry_run = false;
    let mut concurrency = 4;
    let mut token_env = "RAINDROP_TOKEN".to_string();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--out" => out = Some(PathBuf::from(value()?)),
            "--ledger" => ledger = Some(PathBuf::from(value()?)),
            "--ids" => selection = Some(Selection::Ids(parse_ids(&value()?)?)),
            "--newest" => {
                selection = Some(Selection::Newest(parse_positive(&value()?, "--newest")?))
            }
            "--concurrency" => concurrency = parse_positive(&value()?, "--concurrency")?.min(32),
            "--token-env" => token_env = value()?,
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let out = out.ok_or("--out is required")?;
    Ok(Args {
        ledger: ledger.unwrap_or_else(|| out.join("ledger.jsonl")),
        out,
        selection: selection.ok_or("one of --ids or --newest is required")?,
        dry_run,
        concurrency,
        token_env,
    })
}

fn parse_ids(text: &str) -> Result<Vec<u64>, String> {
    let ids: Vec<u64> = text
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().map_err(|_| format!("bad id {s:?}")))
        .collect::<Result<_, _>>()?;
    if ids.is_empty() {
        return Err("--ids needs at least one id".to_string());
    }
    Ok(ids)
}

fn parse_positive(text: &str, flag: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{flag} needs a positive number, got {text:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_ids_and_defaults() {
        let a = args(&["--out", "o", "--ids", "1, 2,3"]).expect("args");
        assert!(matches!(a.selection, Selection::Ids(ref ids) if ids == &[1, 2, 3]));
        assert_eq!(a.ledger, PathBuf::from("o/ledger.jsonl"));
        assert_eq!(a.concurrency, 4);
        assert!(!a.dry_run);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(args(&["--ids", "1"]).is_err());
        assert!(args(&["--out", "o"]).is_err());
        assert!(args(&["--out", "o", "--ids", "x"]).is_err());
        assert!(args(&["--out", "o", "--newest", "0"]).is_err());
        assert!(args(&["--out", "o", "--newest"]).is_err());
        assert!(args(&["--out", "o", "--newest", "5", "--bogus"]).is_err());
    }
}
