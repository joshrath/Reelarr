//! Bounded XML parsing. Namespace URIs, not prefixes, identify Newznab fields.
use crate::{Capabilities, Category, IndexerError, Release, SearchMode, SecretString};
use reqwest::Url;
use roxmltree::{Document, Node, ParsingOptions};
use std::collections::BTreeMap;

const NS: &str = "http://www.newznab.com/DTD/2010/feeds/attributes/";
const BAD: IndexerError = IndexerError::MalformedXml;

pub(crate) fn document(body: &[u8]) -> Result<Document<'_>, IndexerError> {
    let text = std::str::from_utf8(body).map_err(|_| BAD)?;
    let doc = Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 100_000,
            ..Default::default()
        },
    )
    .map_err(|_| BAD)?;
    if doc
        .descendants()
        .any(|n| n.ancestors().take(34).count() > 32)
    {
        return Err(BAD);
    }
    Ok(doc)
}
/// Parse but do not propagate provider descriptions (they may echo the API key).
pub(crate) fn api_error(doc: &Document<'_>) -> Result<(), IndexerError> {
    let root = doc.root_element();
    if root.has_tag_name("error") {
        let code = number(root.attribute("code"))?.ok_or(BAD)?;
        let _description = root.attribute("description");
        return Err(IndexerError::api(code));
    }
    Ok(())
}
fn number<T: std::str::FromStr>(value: Option<&str>) -> Result<Option<T>, IndexerError> {
    value.map(|s| s.parse().map_err(|_| BAD)).transpose()
}
fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children().find(|n| n.has_tag_name(name))
}
fn text(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter_map(|n| n.is_text().then(|| n.text()).flatten())
        .collect::<String>()
        .trim()
        .to_owned()
}
fn mode(node: Option<Node<'_, '_>>) -> SearchMode {
    node.map(|n| SearchMode {
        available: n.attribute("available") == Some("yes"),
        supported_params: n
            .attribute("supportedParams")
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    })
    .unwrap_or_default()
}
pub(crate) fn caps(body: &[u8]) -> Result<Capabilities, IndexerError> {
    let doc = document(body)?;
    api_error(&doc)?;
    let root = doc.root_element();
    if !root.has_tag_name("caps") {
        return Err(BAD);
    }
    let searching = child(root, "searching").ok_or(BAD)?;
    let limits = child(root, "limits");
    let max_limit = number(limits.and_then(|n| n.attribute("max")))?.unwrap_or(100u32);
    let default_limit = number(limits.and_then(|n| n.attribute("default")))?.unwrap_or(100u32);
    if max_limit == 0 || default_limit == 0 || default_limit > max_limit {
        return Err(BAD);
    }
    let mut categories = Vec::new();
    if let Some(list) = child(root, "categories") {
        for n in list
            .descendants()
            .filter(|n| n.has_tag_name("category") || n.has_tag_name("subcat"))
        {
            let id = number(n.attribute("id"))?.ok_or(BAD)?;
            let parent_id = n
                .parent()
                .filter(|p| p.has_tag_name("category") || p.has_tag_name("subcat"))
                .map(|p| number(p.attribute("id")))
                .transpose()?
                .flatten();
            categories.push(Category {
                id,
                name: n.attribute("name").unwrap_or("").into(),
                parent_id,
            });
        }
    }
    Ok(Capabilities {
        search: mode(child(searching, "search")),
        movie: mode(child(searching, "movie-search")),
        categories,
        default_limit,
        max_limit,
    })
}

pub(crate) struct Page {
    pub releases: Vec<Release>,
    pub offset: Option<u64>,
    pub total: Option<u64>,
}

pub(crate) fn page(body: &[u8], endpoint: &Url) -> Result<Page, IndexerError> {
    let doc = document(body)?;
    api_error(&doc)?;
    let root = doc.root_element();
    if !root.has_tag_name("rss") || root.attribute("version") != Some("2.0") {
        return Err(BAD);
    }
    let channel = child(root, "channel").ok_or(BAD)?;
    let response = channel
        .children()
        .find(|n| n.has_tag_name((NS, "response")));
    let (offset, total) = match response {
        Some(n) => (
            Some(number(n.attribute("offset"))?.ok_or(BAD)?),
            Some(number(n.attribute("total"))?.ok_or(BAD)?),
        ),
        None => (None, None),
    };
    let mut releases = Vec::new();
    for item in channel.children().filter(|n| n.has_tag_name("item")) {
        let title = text(child(item, "title").ok_or(BAD)?);
        let guid = text(child(item, "guid").ok_or(BAD)?);
        if title.is_empty() || guid.is_empty() {
            return Err(BAD);
        }
        let enclosure = item
            .children()
            .find(|n| {
                n.has_tag_name("enclosure")
                    && n.attribute("type")
                        .is_none_or(|t| t == "application/x-nzb" || t == "application/octet-stream")
            })
            .ok_or(BAD)?;
        let download = endpoint
            .join(enclosure.attribute("url").ok_or(BAD)?)
            .map_err(|_| IndexerError::UnsafeDownloadUrl)?;
        if download.origin() != endpoint.origin()
            || !download.username().is_empty()
            || download.password().is_some()
            || download.fragment().is_some()
            || !matches!(download.scheme(), "https" | "http")
        {
            return Err(IndexerError::UnsafeDownloadUrl);
        }
        let mut attributes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for attr in item.children().filter(|n| n.has_tag_name((NS, "attr"))) {
            attributes
                .entry(attr.attribute("name").ok_or(BAD)?.to_ascii_lowercase())
                .or_default()
                .push(attr.attribute("value").ok_or(BAD)?.to_owned());
        }
        let first = |key: &str| {
            attributes
                .get(key)
                .and_then(|v| v.first())
                .map(String::as_str)
        };
        let size = number(first("size"))?.or(number(enclosure.attribute("length"))?);
        let categories = attributes
            .get("category")
            .into_iter()
            .flatten()
            .map(|s| s.parse().map_err(|_| BAD))
            .collect::<Result<Vec<u32>, _>>()?;
        let imdb_id = first("imdb").or_else(|| first("imdbid")).map(str::to_owned);
        let tmdb_id = number(first("tmdbid").or_else(|| first("tmdb")))?;
        let grabs = number(first("grabs"))?;
        let files = number(first("files"))?;
        let password = number(first("password"))?;
        let year = number(first("year"))?;
        let pub_date = child(item, "pubDate").map(text);
        releases.push(Release {
            title,
            guid,
            download_url: SecretString::new(download.to_string()),
            size,
            pub_date,
            categories,
            imdb_id,
            tmdb_id,
            grabs,
            files,
            password,
            year,
            attributes,
        });
    }
    Ok(Page {
        releases,
        offset,
        total,
    })
}
