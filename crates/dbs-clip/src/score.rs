//! Paragraph scoring, ported from url2obs's `ContentScorer`.
//!
//! Two additions from Mozilla Readability, which url2obs's scorer is a
//! cut-down form of, because url2obs alone picks the wrong block on
//! common pages (checked on arXiv abstract pages, GitHub gists with
//! comments and table-and-`<br>` essays):
//!
//! * a block with its own text and no block children (an arXiv
//!   `<blockquote class="abstract">`, an old page's `<font>` or `<td>`)
//!   votes like a `<p>`; and
//! * an ancestor whose class/id looks like boilerplate loses as much as a
//!   content-looking one gains, so a `comment-body` (both) earns nothing.

use std::collections::HashMap;

use ego_tree::NodeId;
use scraper::ElementRef;

use crate::dom::Dom;
use crate::patterns;

/// Paragraphs with fewer characters than this do not vote.
const MIN_PARAGRAPH_CHARS: usize = 25;
/// How many ancestors of a paragraph receive its score.
const ANCESTOR_LEVELS: usize = 3;
/// Bonus for an ancestor whose class/id looks like content, and the
/// penalty for one that looks like boilerplate.
const CLASS_WEIGHT: i64 = 25;
/// Tags that vote like a `<p>` when they hold text and no blocks.
const PARAGRAPH_LIKE_TAGS: &[&str] = &["div", "blockquote", "font", "td", "section", "center"];

/// Candidate scores in first-seen order, so ties resolve deterministically
/// to the earliest element, as url2obs's stable sort does.
#[derive(Debug, Default)]
pub(crate) struct Scores {
    order: Vec<NodeId>,
    by_id: HashMap<NodeId, i64>,
}

impl Scores {
    pub(crate) fn get(&self, id: NodeId) -> Option<i64> {
        self.by_id.get(&id).copied()
    }

    /// The highest-scoring candidate; the earliest one wins a tie.
    pub(crate) fn best(&self) -> Option<(NodeId, i64)> {
        let mut best: Option<(NodeId, i64)> = None;
        for id in &self.order {
            let score = self.by_id[id];
            if best.is_none_or(|(_, top)| score > top) {
                best = Some((*id, score));
            }
        }
        best
    }

    fn add(&mut self, id: NodeId, points: i64) {
        let entry = self.by_id.entry(id).or_insert_with(|| {
            self.order.push(id);
            0
        });
        *entry += points;
    }
}

/// Scores the ancestors of every live `<p>`.
pub(crate) fn score_paragraphs(dom: &Dom) -> Scores {
    let mut scores = Scores::default();
    let paragraphs = dom
        .elements()
        .filter(|el| dom.is_live(**el) && votes(dom, *el));
    for p in paragraphs {
        let text = dom.stripped_text(p);
        if text.chars().count() < MIN_PARAGRAPH_CHARS {
            continue;
        }
        credit_ancestors(&mut scores, p, text_score(&text));
    }
    scores
}

/// A `<p>`, or a paragraph-like block: own text, no live block
/// children, and not inside a `<p>` (which votes already).
fn votes(dom: &Dom, el: ElementRef<'_>) -> bool {
    let name = el.value().name();
    if name == "p" {
        return true;
    }
    if !PARAGRAPH_LIKE_TAGS.contains(&name) {
        return false;
    }
    let has_own_text = el
        .children()
        .any(|n| n.value().as_text().is_some_and(|t| !t.trim().is_empty()));
    let has_blocks = dom
        .live_children(el)
        .any(|c| patterns::BLOCK_TAGS.contains(&c.value().name()));
    let in_paragraph = el
        .ancestors()
        .filter_map(ElementRef::wrap)
        .any(|a| a.value().name() == "p");
    has_own_text && !has_blocks && !in_paragraph
}

/// url2obs walks three levels up; an unlikely tag uses up a level
/// without being scored. Only elements score: the document root never
/// becomes a candidate.
fn credit_ancestors(scores: &mut Scores, p: ElementRef<'_>, points: i64) {
    let ancestors = p
        .ancestors()
        .take(ANCESTOR_LEVELS)
        .filter_map(ElementRef::wrap);
    for parent in ancestors {
        if patterns::is_unlikely_tag(parent.value().name()) {
            continue;
        }
        scores.add(parent.id(), points + class_weight(parent));
    }
}

fn class_weight(el: ElementRef<'_>) -> i64 {
    let attrs = patterns::attr_string(el);
    let mut weight = 0;
    if patterns::is_positive(&attrs) {
        weight += CLASS_WEIGHT;
    }
    if patterns::is_negative(&attrs) {
        weight -= CLASS_WEIGHT;
    }
    weight
}

/// url2obs's `_calculate_text_score`: length, commas, quotes, sentences.
pub(crate) fn text_score(text: &str) -> i64 {
    let count = |c: char| text.chars().filter(|x| *x == c).count() as i64;
    let length_points = (text.chars().count() as i64 / 100).min(3);
    1 + length_points + count(',') * 3 + count('"') * 2 + count('.').min(5) * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_score_matches_url2obs() {
        // 1 + 0 (short) + 2 commas*3 + 1 quote*2 + 2 periods*2
        assert_eq!(text_score("a, b, \"c. d."), 1 + 6 + 2 + 4);
        // periods are capped at five
        assert_eq!(text_score(".........."), 1 + 10);
        // length points are capped at three
        assert_eq!(text_score(&"x".repeat(1000)), 1 + 3);
    }

    #[test]
    fn short_paragraphs_do_not_vote_and_the_content_div_wins() {
        let dom = Dom::parse_and_clean(
            r#"<body><div id="a"><p>too short</p></div>
               <div id="b" class="entry-content"><p>This paragraph is long enough, with commas, to count.</p></div></body>"#,
        );
        let scores = score_paragraphs(&dom);
        let (best, _) = scores.best().expect("a candidate");
        let el = dom.element(best).expect("element");
        assert_eq!(el.value().id(), Some("b"));
    }

    #[test]
    fn text_blocks_without_paragraphs_vote() {
        let dom = Dom::parse_and_clean(
            r#"<body><div id="w"><blockquote id="q">An abstract with no paragraph tags, but commas, and full stops.</blockquote></div></body>"#,
        );
        let (best, _) = score_paragraphs(&dom).best().expect("a candidate");
        assert_eq!(dom.element(best).and_then(|e| e.value().id()), Some("w"));
    }

    #[test]
    fn boilerplate_classes_cancel_the_content_bonus() {
        let dom = Dom::parse_and_clean(
            r#"<body><div id="a" class="comment-body">{P}</div><div id="b" class="entry">{P}</div></body>"#
                .replace("{P}", "<p>Long enough to vote, with a comma, and a stop.</p>")
                .as_str(),
        );
        let scores = score_paragraphs(&dom);
        let id_of = |name: &str| {
            dom.elements()
                .find(|e| e.value().id() == Some(name))
                .map(|e| e.id())
        };
        let a = scores.get(id_of("a").expect("a")).expect("a scored");
        let b = scores.get(id_of("b").expect("b")).expect("b scored");
        assert_eq!(b - a, CLASS_WEIGHT);
    }

    #[test]
    fn no_paragraphs_means_no_candidate() {
        let dom = Dom::parse_and_clean("<body><script>render()</script><div id=root></div></body>");
        assert!(score_paragraphs(&dom).best().is_none());
    }
}
