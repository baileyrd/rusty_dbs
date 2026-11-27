//! Page metadata, ported from url2obs's `MetadataExtractor`.

use chrono::{DateTime, NaiveDate};
use scraper::{Html, Selector};

/// Metadata read from a page's `<head>`. Empty strings mean "absent".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageMeta {
    pub author: String,
    pub title: String,
    pub description: String,
    pub published: Option<NaiveDate>,
    pub keywords: Vec<String>,
}

/// Date sources tried in order, as url2obs does.
const DATE_PROPS: &[&str] = &[
    "article:published_time",
    "og:published_time",
    "datePublished",
];

pub(crate) fn extract(doc: &Html) -> PageMeta {
    PageMeta {
        author: author(doc),
        title: first_text(doc, "title"),
        description: meta_content(doc, "meta[name=\"description\"]"),
        published: published(doc),
        keywords: keywords(doc),
    }
}

fn author(doc: &Html) -> String {
    [
        "meta[name=\"author\"]",
        "meta[property=\"author\"]",
        "meta[property=\"og:site_name\"]",
    ]
    .iter()
    .map(|sel| meta_content(doc, sel))
    .find(|value| !value.is_empty())
    .unwrap_or_default()
}

fn published(doc: &Html) -> Option<NaiveDate> {
    let from_meta = DATE_PROPS.iter().find_map(|prop| {
        let by_property = meta_content(doc, &format!("meta[property=\"{prop}\"]"));
        let by_itemprop = meta_content(doc, &format!("meta[itemprop=\"{prop}\"]"));
        parse_date(&by_property).or_else(|| parse_date(&by_itemprop))
    });
    from_meta.or_else(|| {
        let time = select(doc, "time[datetime]").next()?;
        parse_date(time.value().attr("datetime")?)
    })
}

fn keywords(doc: &Html) -> Vec<String> {
    meta_content(doc, "meta[name=\"keywords\" i]")
        .split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect()
}

/// Accepts RFC 3339 timestamps and bare `YYYY-MM-DD` dates (also as the
/// prefix of a looser timestamp). Anything else is treated as absent.
pub(crate) fn parse_date(raw: &str) -> Option<NaiveDate> {
    let raw = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.date_naive());
    }
    let prefix = raw.get(..10)?;
    NaiveDate::parse_from_str(prefix, "%Y-%m-%d").ok()
}

fn meta_content(doc: &Html, selector: &str) -> String {
    select(doc, selector)
        .find_map(|el| el.value().attr("content"))
        .map(|c| c.trim().to_string())
        .unwrap_or_default()
}

fn first_text(doc: &Html, selector: &str) -> String {
    select(doc, selector)
        .next()
        .map(|el| collapse_whitespace(&el.text().collect::<String>()))
        .unwrap_or_default()
}

pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Selectors here are compile-time literals; an invalid one is a bug, and
/// yields no matches rather than a panic.
fn select<'a>(doc: &'a Html, selector: &str) -> impl Iterator<Item = scraper::ElementRef<'a>> {
    let parsed = Selector::parse(selector).ok();
    parsed
        .into_iter()
        .flat_map(move |sel| doc.select(&sel).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_of(head: &str) -> PageMeta {
        extract(&Html::parse_document(&format!(
            "<html><head>{head}</head><body></body></html>"
        )))
    }

    #[test]
    fn reads_every_field() {
        let m = meta_of(
            r#"<title> A  Title </title>
               <meta name="author" content="Ada">
               <meta name="description" content="About it">
               <meta property="article:published_time" content="2024-03-05T10:00:00Z">
               <meta name="Keywords" content="rust, html , ,clip">"#,
        );
        assert_eq!(m.title, "A Title");
        assert_eq!(m.author, "Ada");
        assert_eq!(m.description, "About it");
        assert_eq!(m.published, NaiveDate::from_ymd_opt(2024, 3, 5));
        assert_eq!(m.keywords, ["rust", "html", "clip"]);
    }

    #[test]
    fn author_falls_back_to_site_name() {
        let m = meta_of(r#"<meta property="og:site_name" content="Example">"#);
        assert_eq!(m.author, "Example");
    }

    #[test]
    fn published_falls_back_to_time_element_and_skips_garbage() {
        let doc = Html::parse_document(
            r#"<head><meta property="article:published_time" content="soon"></head>
               <body><time datetime="2015-07-01">July</time></body>"#,
        );
        assert_eq!(published(&doc), NaiveDate::from_ymd_opt(2015, 7, 1));
    }

    #[test]
    fn missing_metadata_is_empty() {
        let m = meta_of("");
        assert_eq!(m.title, "");
        assert_eq!(m.author, "");
        assert_eq!(m.published, None);
        assert!(m.keywords.is_empty());
    }
}
