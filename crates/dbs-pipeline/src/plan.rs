//! Dry run: what a batch would do, without fetching or writing.

use std::fmt;

use crate::ledger::Ledger;
use crate::source::Listing;

/// What would happen to one bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Fetch,
    AlreadySettled,
    DuplicateOf(u64),
    Rejected(String),
}

/// One line of a dry-run plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub id: Option<u64>,
    pub url: String,
    pub action: Action,
}

impl fmt::Display for Planned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id = self
            .id
            .map(|i| i.to_string())
            .unwrap_or_else(|| "-".to_string());
        let action = match &self.action {
            Action::Fetch => "fetch".to_string(),
            Action::AlreadySettled => "settled earlier".to_string(),
            Action::DuplicateOf(owner) => format!("duplicate of {owner}"),
            Action::Rejected(reason) => format!("rejected: {reason}"),
        };
        write!(f, "{id}\t{action}\t{}", self.url)
    }
}

/// Plans the listing against an in-memory ledger (never written).
pub fn plan(ledger: &mut Ledger, listing: &Listing) -> Vec<Planned> {
    let mut lines: Vec<Planned> = listing
        .bookmarks
        .iter()
        .map(|b| {
            let action = if ledger.is_settled(b.id) {
                Action::AlreadySettled
            } else {
                ledger
                    .claim_url(b.id, &b.link)
                    .map_or(Action::Fetch, Action::DuplicateOf)
            };
            Planned {
                id: Some(b.id),
                url: b.link.to_string(),
                action,
            }
        })
        .collect();
    lines.extend(listing.rejected.iter().map(|r| Planned {
        id: r.id,
        url: String::new(),
        action: Action::Rejected(r.reason.clone()),
    }));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Bookmark, Rejected};
    use url::Url;

    fn bookmark(id: u64, link: &str) -> Bookmark {
        Bookmark {
            id,
            link: Url::parse(link).expect("url"),
            title: String::new(),
            tags: vec![],
            saved: None,
            highlights: vec![],
        }
    }

    #[test]
    fn plans_fetch_duplicate_and_rejected_without_writing() {
        let (mut ledger, _) =
            Ledger::read_only(std::path::Path::new("/nonexistent/ledger.jsonl")).expect("empty");
        let listing = Listing {
            bookmarks: vec![
                bookmark(1, "https://e.com/a"),
                bookmark(2, "https://www.e.com/a/?utm_medium=x"),
            ],
            rejected: vec![Rejected {
                id: Some(3),
                reason: "missing or non-http(s) link".into(),
            }],
        };
        let lines: Vec<String> = plan(&mut ledger, &listing)
            .iter()
            .map(Planned::to_string)
            .collect();
        assert_eq!(
            lines,
            [
                "1\tfetch\thttps://e.com/a",
                "2\tduplicate of 1\thttps://www.e.com/a/?utm_medium=x",
                "3\trejected: missing or non-http(s) link\t",
            ]
        );
    }
}
