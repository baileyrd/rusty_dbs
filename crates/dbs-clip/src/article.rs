//! Article selection and final cleanup, ported from url2obs's
//! `ContentFormatter._extract_article_content` and `_final_clean`.

use ego_tree::NodeId;
use scraper::ElementRef;

use crate::dom::Dom;
use crate::patterns;
use crate::score::Scores;

/// A sibling joins the article if it scores at least this share of the
/// top candidate (and at least [`MIN_SIBLING_SCORE`]).
const SIBLING_SHARE: f64 = 0.2;
const MIN_SIBLING_SCORE: f64 = 10.0;

/// The top candidate followed by its qualifying next siblings, or `None`
/// when no paragraph scored.
pub(crate) fn select_roots(dom: &Dom, scores: &Scores) -> Option<Vec<NodeId>> {
    let (top_id, top_score) = scores.best()?;
    let top = dom.element(top_id)?;
    if top_score <= 0 {
        return None;
    }
    let threshold = MIN_SIBLING_SCORE.max(top_score as f64 * SIBLING_SHARE);
    let siblings = top
        .next_siblings()
        .filter_map(ElementRef::wrap)
        .filter(|s| dom.is_live(**s) && !patterns::is_unlikely_tag(s.value().name()))
        .filter(|s| {
            scores
                .get(s.id())
                .is_some_and(|score| score as f64 >= threshold)
        })
        .map(|s| s.id());
    Some(std::iter::once(top_id).chain(siblings).collect())
}

/// url2obs's `_final_clean`, in its order: empty paragraphs, then
/// boilerplate by class/id, then empty divs. Code is never touched.
///
/// One deliberate difference: url2obs can remove the article root itself
/// here (its final pass ignores positive patterns, so a root such as
/// `class="article-meta"` that survived the first clean would vanish).
/// Roots are exempt, so a chosen article is never emptied by cleanup.
pub(crate) fn final_clean(dom: &mut Dom, roots: &[NodeId]) {
    let empty_paragraphs = collect(dom, roots, |dom, el| {
        el.value().name() == "p" && !dom.has_text(el)
    });
    remove_all(dom, empty_paragraphs);

    let boilerplate = collect(dom, roots, |_, el| {
        let attrs = patterns::attr_string(el);
        !roots.contains(&el.id()) && patterns::is_negative(&attrs) && !patterns::is_code_element(el)
    });
    remove_all(dom, boilerplate);

    let empty_divs = collect(dom, roots, |dom, el| {
        el.value().name() == "div" && !dom.has_text(el) && dom.live_children(el).next().is_none()
    });
    remove_all(dom, empty_divs);
}

/// Live, unprotected elements under `roots` that match `doomed`.
fn collect(
    dom: &Dom,
    roots: &[NodeId],
    doomed: impl Fn(&Dom, ElementRef<'_>) -> bool,
) -> Vec<NodeId> {
    roots
        .iter()
        .filter_map(|id| dom.element(*id))
        .flat_map(|root| dom.live_elements(root).collect::<Vec<_>>())
        .filter(|el| !dom.is_protected(el.id()) && doomed(dom, *el))
        .map(|el| el.id())
        .collect()
}

fn remove_all(dom: &mut Dom, ids: Vec<NodeId>) {
    for id in ids {
        dom.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::score_paragraphs;

    const PARA: &str = "<p>A paragraph long enough to vote, with a comma, and a full stop.</p>";

    fn roots_of(html: &str) -> (Dom, Vec<NodeId>) {
        let dom = Dom::parse_and_clean(html);
        let roots = select_roots(&dom, &score_paragraphs(&dom)).expect("roots");
        (dom, roots)
    }

    fn ids(dom: &Dom, roots: &[NodeId]) -> Vec<String> {
        roots
            .iter()
            .filter_map(|id| dom.element(*id))
            .filter_map(|el| el.value().id().map(str::to_string))
            .collect()
    }

    /// Paragraphs three levels down, so the `post` div is the last
    /// ancestor credited and its positive class makes it the top.
    fn post(id: &str, paras: usize) -> String {
        format!(
            r#"<div id="{id}" class="post"><div><div>{}</div></div></div>"#,
            PARA.repeat(paras)
        )
    }

    #[test]
    fn strong_siblings_join_the_top_candidate_and_weak_ones_do_not() {
        let html = format!(
            "<body><div>{}{}<div id=\"c\"><p>tiny</p></div>{}</div></body>",
            post("a", 3),
            post("b", 2),
            post("d", 1)
        );
        let (dom, roots) = roots_of(&html);
        assert_eq!(ids(&dom, &roots), ["a", "b", "d"]);
    }

    #[test]
    fn a_page_without_scoring_paragraphs_has_no_roots() {
        let dom = Dom::parse_and_clean("<body><p>short</p></body>");
        assert!(select_roots(&dom, &score_paragraphs(&dom)).is_none());
    }

    #[test]
    fn final_clean_removes_empties_and_boilerplate_but_not_roots_or_code() {
        let html = format!(
            r#"<body><div id="r" class="article-meta">{PARA}<p id="e"> </p>
               <div id="m" class="post-meta">by x</div><div id="d"></div>
               <div id="k" class="share"><pre>keep()</pre></div></div></body>"#
        );
        let (mut dom, roots) = roots_of(&html);
        final_clean(&mut dom, &roots);
        let root = dom.element(roots[0]).expect("root");
        let live: Vec<String> = dom
            .live_elements(root)
            .filter_map(|el| el.value().id().map(str::to_string))
            .collect();
        assert_eq!(live, ["r", "k"]);
    }
}
