//! A parsed page plus the set of pruned subtrees, ported from url2obs's
//! `HtmlCleaner`.
//!
//! url2obs mutates its BeautifulSoup tree (`decompose`). `scraper`'s tree
//! is read-mostly, so pruning here records removed subtree roots instead
//! and every later pass asks [`Dom::is_live`]. The outcome is the same:
//! a removed element and everything under it no longer exists for
//! scoring or rendering.

use std::collections::HashSet;

use ego_tree::{NodeId, NodeRef};
use scraper::{ElementRef, Html, Node};

use crate::patterns;

pub(crate) struct Dom {
    pub(crate) doc: Html,
    removed: HashSet<NodeId>,
    /// Code and instruction elements: never removed.
    preserved: HashSet<NodeId>,
    /// Ancestors of preserved elements: never removed either, because
    /// removing them would take the code with them.
    guarding: HashSet<NodeId>,
}

impl Dom {
    /// Parses `html`, marks code to keep, and prunes boilerplate
    /// (url2obs's `clean_page`).
    pub(crate) fn parse_and_clean(html: &str) -> Self {
        let doc = Html::parse_document(html);
        let mut dom = Dom {
            doc,
            removed: HashSet::new(),
            preserved: HashSet::new(),
            guarding: HashSet::new(),
        };
        dom.mark_preserved();
        dom.prune();
        dom
    }

    fn mark_preserved(&mut self) {
        let keep: Vec<NodeId> = self
            .elements()
            .filter(|el| should_preserve(*el))
            .map(|el| el.id())
            .collect();
        for id in keep {
            self.preserve(id);
        }
    }

    fn preserve(&mut self, id: NodeId) {
        self.preserved.insert(id);
        let Some(node) = self.doc.tree.get(id) else {
            return;
        };
        let ancestors: Vec<NodeId> = node.ancestors().map(|a| a.id()).collect();
        self.guarding.extend(ancestors);
    }

    fn prune(&mut self) {
        let doomed: Vec<NodeId> = self
            .elements()
            .filter(|el| self.prunable(*el))
            .map(|el| el.id())
            .collect();
        self.removed.extend(doomed);
    }

    fn prunable(&self, el: ElementRef<'_>) -> bool {
        let name = el.value().name();
        if patterns::ALWAYS_REMOVED_TAGS.contains(&name) {
            return true;
        }
        if self.is_protected(el.id()) {
            return false;
        }
        if patterns::is_unlikely_tag(name) {
            return true;
        }
        let attrs = patterns::attr_string(el);
        patterns::is_negative(&attrs)
            && !patterns::is_positive(&attrs)
            && !patterns::is_code_element(el)
    }

    /// True if the element is code, or holds code.
    pub(crate) fn is_protected(&self, id: NodeId) -> bool {
        self.preserved.contains(&id) || self.guarding.contains(&id)
    }

    pub(crate) fn remove(&mut self, id: NodeId) {
        self.removed.insert(id);
    }

    /// True unless the node or one of its ancestors was removed.
    pub(crate) fn is_live(&self, node: NodeRef<'_, Node>) -> bool {
        !self.removed.contains(&node.id())
            && !node.ancestors().any(|a| self.removed.contains(&a.id()))
    }

    /// Every element in document order.
    pub(crate) fn elements(&self) -> impl Iterator<Item = ElementRef<'_>> {
        self.doc
            .tree
            .root()
            .descendants()
            .filter_map(ElementRef::wrap)
    }

    /// Live descendant elements of `root` (inclusive) in document order.
    pub(crate) fn live_elements<'a>(
        &'a self,
        root: ElementRef<'a>,
    ) -> impl Iterator<Item = ElementRef<'a>> + 'a {
        root.descendants()
            .filter_map(ElementRef::wrap)
            .filter(move |el| self.is_live(**el))
    }

    /// Live element children of `el`.
    pub(crate) fn live_children<'a>(
        &'a self,
        el: ElementRef<'a>,
    ) -> impl Iterator<Item = ElementRef<'a>> + 'a {
        el.children()
            .filter_map(ElementRef::wrap)
            .filter(move |c| !self.removed.contains(&c.id()))
    }

    /// The live text under `el`, each text node trimmed and concatenated:
    /// BeautifulSoup's `get_text(strip=True)`, used for scoring.
    pub(crate) fn stripped_text(&self, el: ElementRef<'_>) -> String {
        el.descendants()
            .filter(|n| self.is_live(*n))
            .filter_map(|n| n.value().as_text().map(|t| t.trim().to_string()))
            .collect()
    }

    pub(crate) fn has_text(&self, el: ElementRef<'_>) -> bool {
        el.descendants()
            .filter(|n| self.is_live(*n))
            .any(|n| n.value().as_text().is_some_and(|t| !t.trim().is_empty()))
    }

    pub(crate) fn element(&self, id: NodeId) -> Option<ElementRef<'_>> {
        self.doc.tree.get(id).and_then(ElementRef::wrap)
    }
}

fn should_preserve(el: ElementRef<'_>) -> bool {
    let is_instruction_block =
        matches!(el.value().name(), "div" | "section") && patterns::is_instruction(el);
    is_instruction_block || patterns::is_code_element(el)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scraper::Selector;

    fn live_ids(dom: &Dom, sel: &str) -> Vec<String> {
        let sel = Selector::parse(sel).expect("test selector");
        dom.doc
            .select(&sel)
            .filter(|el| dom.is_live(**el))
            .filter_map(|el| el.value().id().map(str::to_string))
            .collect()
    }

    #[test]
    fn prunes_boilerplate_but_keeps_positive_and_code() {
        let dom = Dom::parse_and_clean(
            r#"<body>
                <nav id="n1">menu</nav>
                <div id="d1" class="sidebar">side</div>
                <div id="d2" class="post-meta">kept: positive wins at clean time</div>
                <div id="d3" class="share-bar"><pre>code</pre></div>
                <aside id="d4"><code>x</code></aside>
                <div id="d5" class="entry">body</div>
                <script id="s1">alert(1)</script>
            </body>"#,
        );
        assert_eq!(live_ids(&dom, "[id]"), ["d2", "d3", "d4", "d5"]);
    }

    #[test]
    fn stripped_text_skips_removed_subtrees() {
        let dom = Dom::parse_and_clean(
            r#"<body><p id="p"> Hello <span class="social">x</span><b> world </b></p></body>"#,
        );
        let sel = Selector::parse("#p").expect("test selector");
        let p = dom.doc.select(&sel).next().expect("p exists");
        assert_eq!(dom.stripped_text(p), "Helloworld");
    }
}
