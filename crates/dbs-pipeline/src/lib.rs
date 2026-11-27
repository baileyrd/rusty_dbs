//! Knowledge pipeline, first stages (Mill Board #1308, parent #763):
//! Raindrop bookmarks are fetched (live page, then Raindrop's stored copy,
//! then the Wayback Machine), clipped to markdown by `dbs-clip`, and
//! written one file per bookmark id, with every outcome recorded in an
//! append-only ledger so batches resume.
//!
//! Ports and adapters: [`source::BookmarkSource`] (Raindrop adapter
//! [`source::RaindropApi`]), the fetch chain [`fetch::Fetcher`] over a
//! [`fetch::CopyStore`], the [`ledger::Ledger`], and [`runner::run`]
//! which composes them.

pub mod fetch;
pub mod ledger;
pub mod plan;
pub mod runner;
pub mod source;
