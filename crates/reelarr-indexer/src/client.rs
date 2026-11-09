//! Shared asynchronous transport, capability cache, rate gate and bounded pagination.
use crate::{Capabilities, IndexerConfig, IndexerError, MovieQuery, Release, SearchResults, xml};
use reqwest::{Client, Url, redirect::Policy};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};

/// Redirect hops followed by [`NewznabClient::fetch_nzb`] before failing closed.
pub const MAX_DOWNLOAD_REDIRECTS: usize = 5;

struct Gate {
    next: Instant,
    cooldown: Option<(Instant, Duration)>,
}
struct Inner {
    config: IndexerConfig,
    endpoint: Url,
    http: Client,
    caps: Mutex<Option<(Instant, Capabilities)>>,
    gate: Mutex<Gate>,
}
/// Clone this handle to share connection pooling, cached capabilities and the rate gate.
/// Do not construct a new client per search: that would reset the per-indexer quota.
#[derive(Clone)]
pub struct NewznabClient {
    inner: Arc<Inner>,
}
impl NewznabClient {
    /// Validate settings and construct a rustls-only, non-redirecting HTTP client.
    pub fn new(config: IndexerConfig) -> Result<Self, IndexerError> {
        config.validate()?;
        let endpoint = config
            .base_url
            .join(&config.api_path)
            .map_err(|_| IndexerError::InvalidConfig)?;
        let http = Client::builder()
            .tls_backend_rustls()
            .redirect(Policy::none())
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .no_proxy()
            .user_agent("Reelarr/0.0.1")
            .build()
            .map_err(IndexerError::transport)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                endpoint,
                http,
                caps: Mutex::new(None),
                gate: Mutex::new(Gate {
                    next: Instant::now(),
                    cooldown: None,
                }),
            }),
        })
    }
    /// Explicitly invalidate capabilities (e.g. after an administrator requests a refresh).
    pub async fn invalidate_capabilities(&self) {
        *self.inner.caps.lock().await = None;
    }
    /// Discover `t=caps`; concurrent calls share one in-flight refresh and TTL cache.
    pub async fn capabilities(&self) -> Result<Capabilities, IndexerError> {
        let mut cache = self.inner.caps.lock().await;
        if let Some((at, caps)) = cache.as_ref()
            && at.elapsed() < self.inner.config.caps_ttl
        {
            return Ok(caps.clone());
        }
        let caps = xml::caps(&self.request(&[("t".into(), "caps".into())]).await?)?;
        *cache = Some((Instant::now(), caps.clone()));
        Ok(caps)
    }
    /// Search advertised movie IDs/query first; fall back to title/year general search
    /// when unsupported or empty. Authentication, quota and transport errors do not
    /// trigger alternate requests. Truncated results never trigger fallback.
    pub async fn search_movie(&self, query: &MovieQuery) -> Result<SearchResults, IndexerError> {
        if !self.inner.config.enabled {
            return Err(IndexerError::Disabled);
        }
        let imdb = query
            .imdb_id
            .as_deref()
            .map(|id| id.strip_prefix("tt").unwrap_or(id));
        if imdb.is_some_and(|id| {
            id.is_empty() || id.len() > 16 || !id.bytes().all(|b| b.is_ascii_digit())
        }) || query.tmdb_id == Some(0)
            || query.limit == Some(0)
            || query.title.len() > 1024
            || query.title.chars().any(char::is_control)
            || query.year.is_some_and(|y| !(1800..=9999).contains(&y))
        {
            return Err(IndexerError::InvalidQuery);
        }
        let caps = self.capabilities().await?;
        let mut movie_params = vec![("t".into(), "movie".into())];
        let supports = |name: &str| caps.movie.supported_params.contains(name);
        let title = match query.year {
            Some(year) => format!("{} {year}", query.title.trim()),
            None => query.title.trim().to_owned(),
        };
        let has_title = !query.title.trim().is_empty();
        let use_movie = if !caps.movie.available {
            false
        } else if let Some(id) = imdb.filter(|_| supports("imdbid")) {
            movie_params.push(("imdbid".into(), id.into()));
            true
        } else if let Some(id) = query.tmdb_id.filter(|_| supports("tmdbid")) {
            movie_params.push(("tmdbid".into(), id.to_string()));
            true
        } else if has_title && supports("q") {
            movie_params.push(("q".into(), title.clone()));
            true
        } else {
            false
        };
        let mut movie_empty = false;
        if use_movie {
            match self.paginate(movie_params, query, &caps).await {
                Ok(results) if !results.releases.is_empty() || results.truncated => {
                    return Ok(results);
                }
                Ok(_) => movie_empty = true,
                Err(IndexerError::Unsupported) => {}
                Err(e) => return Err(e),
            }
        }
        if !caps.search.available || !has_title || !caps.search.supported_params.contains("q") {
            return if movie_empty {
                Ok(SearchResults {
                    releases: vec![],
                    truncated: false,
                })
            } else {
                Err(IndexerError::Unsupported)
            };
        }
        self.paginate(
            vec![("t".into(), "search".into()), ("q".into(), title)],
            query,
            &caps,
        )
        .await
    }
    async fn paginate(
        &self,
        mut params: Vec<(String, String)>,
        query: &MovieQuery,
        caps: &Capabilities,
    ) -> Result<SearchResults, IndexerError> {
        let config = &self.inner.config;
        let limit = query
            .limit
            .unwrap_or(caps.default_limit)
            .min(caps.max_limit)
            .min(1000);
        params.push(("limit".into(), limit.to_string()));
        // Category filtering is supported by the base Newznab protocol. Unknown
        // configured categories are intentionally sent (the API treats them as no match).
        if !config.categories.is_empty() {
            params.push((
                "cat".into(),
                config
                    .categories
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ));
        }
        let mut offset = query.offset;
        let mut seen = HashSet::new();
        let mut releases = Vec::new();
        for _ in 0..config.max_pages {
            let mut page_params = params.clone();
            page_params.push(("offset".into(), offset.to_string()));
            let page = xml::page(
                &self.request(&page_params).await?,
                &self.inner.endpoint,
                &self.inner.config,
            )?;
            if page.offset.is_some_and(|reported| reported != offset) {
                return Ok(SearchResults {
                    releases,
                    truncated: true,
                });
            }
            let count = page.releases.len();
            let before = releases.len();
            let mut dropped = false;
            for release in page.releases {
                if seen.insert(release.guid.clone()) {
                    if releases.len() < config.max_results {
                        releases.push(release);
                    } else {
                        dropped = true;
                    }
                }
            }
            let Some(next) = offset.checked_add(count as u64) else {
                return Ok(SearchResults {
                    releases,
                    truncated: true,
                });
            };
            let exhausted = count == 0
                || page.total.is_some_and(|total| next >= total)
                || (page.total.is_none() && count < limit as usize);
            if dropped {
                return Ok(SearchResults {
                    releases,
                    truncated: true,
                });
            }
            if count > 0 && releases.len() == before {
                return Ok(SearchResults {
                    releases,
                    truncated: true,
                });
            }
            if exhausted {
                let inconsistent_empty =
                    count == 0 && page.total.is_some_and(|total| offset < total);
                return Ok(SearchResults {
                    releases,
                    truncated: inconsistent_empty,
                });
            }
            if releases.len() >= config.max_results {
                return Ok(SearchResults {
                    releases,
                    truncated: true,
                });
            }
            offset = next;
        }
        Ok(SearchResults {
            releases,
            truncated: true,
        })
    }
    async fn request(&self, params: &[(String, String)]) -> Result<Vec<u8>, IndexerError> {
        if !self.inner.config.enabled {
            return Err(IndexerError::Disabled);
        }
        tokio::time::timeout(
            self.inner.config.request_timeout,
            self.request_inner(params),
        )
        .await
        .map_err(|_| IndexerError::Timeout)?
    }
    async fn request_inner(&self, params: &[(String, String)]) -> Result<Vec<u8>, IndexerError> {
        // Serialize across the entire exchange, not just the send, and do not
        // automatically retry: callers can present typed errors without request storms.
        let mut gate = self.gate().await?;
        let mut request = self
            .inner
            .http
            .get(self.inner.endpoint.clone())
            .query(params);
        // Add only here; neither URLs nor reqwest errors ever enter diagnostics.
        if params
            .iter()
            .any(|(key, value)| key == "t" && value != "caps")
        {
            request = request.query(&[("apikey", self.inner.config.api_key.expose())]);
        }
        let mut response = request.send().await.map_err(IndexerError::transport)?;
        let status = response.status();
        let retry_after = rate_limit(&mut gate, &response)?;
        let body = self.read_body(&mut response).await?;
        // Newznab errors take precedence even when wrapped in a non-200 status.
        if let Ok(doc) = xml::document(&body)
            && let Err(error) = xml::api_error(&doc)
        {
            if let IndexerError::RateLimited {
                retry_after: default,
            } = error
            {
                let delay = retry_after.unwrap_or(default);
                gate.cooldown = Some((Instant::now(), delay));
                return Err(IndexerError::RateLimited { retry_after: delay });
            }
            return Err(error);
        }
        if !status.is_success() {
            return Err(IndexerError::Http {
                status: status.as_u16(),
            });
        }
        Ok(body)
    }
    /// Fetch a release's NZB from its enclosure URL, honoring this indexer's
    /// download-host policy on the initial URL and on every redirect hop.
    ///
    /// Redirects are followed manually, at most [`MAX_DOWNLOAD_REDIRECTS`] times;
    /// each `Location` must itself pass [`IndexerConfig::download_url_allowed`].
    /// The body is capped by `max_response_bytes`, the rate gate and request
    /// timeout apply to the whole exchange, and only a light NZB sanity check is
    /// made here; full parsing belongs to reelarr-nzb. Errors never carry URLs.
    pub async fn fetch_nzb(&self, release: &Release) -> Result<Vec<u8>, IndexerError> {
        if !self.inner.config.enabled {
            return Err(IndexerError::Disabled);
        }
        tokio::time::timeout(
            self.inner.config.request_timeout,
            self.fetch_nzb_inner(release),
        )
        .await
        .map_err(|_| IndexerError::Timeout)?
    }
    async fn fetch_nzb_inner(&self, release: &Release) -> Result<Vec<u8>, IndexerError> {
        let config = &self.inner.config;
        let mut url = Url::parse(release.download_url.expose())
            .map_err(|_| IndexerError::UnsafeDownloadUrl)?;
        let mut gate = self.gate().await?;
        for _ in 0..=MAX_DOWNLOAD_REDIRECTS {
            if !config.download_url_allowed(&self.inner.endpoint, &url) {
                return Err(IndexerError::UnsafeDownloadUrl);
            }
            let mut response = self
                .inner
                .http
                .get(url.clone())
                .send()
                .await
                .map_err(IndexerError::transport)?;
            let status = response.status();
            if status.is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or(IndexerError::Http {
                        status: status.as_u16(),
                    })?;
                url = url
                    .join(location)
                    .map_err(|_| IndexerError::UnsafeDownloadUrl)?;
                continue;
            }
            rate_limit(&mut gate, &response)?;
            let body = self.read_body(&mut response).await?;
            if let Ok(doc) = xml::document(&body) {
                xml::api_error(&doc)?;
            }
            if !status.is_success() {
                return Err(IndexerError::Http {
                    status: status.as_u16(),
                });
            }
            if !looks_like_nzb(&body) {
                return Err(IndexerError::InvalidNzb);
            }
            return Ok(body);
        }
        Err(IndexerError::TooManyRedirects)
    }
    /// Serialize on the per-indexer gate, honoring cooldown and minimum interval.
    async fn gate(&self) -> Result<MutexGuard<'_, Gate>, IndexerError> {
        let mut gate = self.inner.gate.lock().await;
        if let Some((at, duration)) = gate.cooldown {
            let remaining = duration.saturating_sub(at.elapsed());
            if !remaining.is_zero() {
                return Err(IndexerError::RateLimited {
                    retry_after: remaining,
                });
            }
            gate.cooldown = None;
        }
        tokio::time::sleep_until(gate.next).await;
        gate.next = Instant::now() + self.inner.config.minimum_interval;
        Ok(gate)
    }
    async fn read_body(&self, response: &mut reqwest::Response) -> Result<Vec<u8>, IndexerError> {
        let max = self.inner.config.max_response_bytes;
        if response
            .content_length()
            .is_some_and(|len| len > max as u64)
        {
            return Err(IndexerError::ResponseTooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(IndexerError::transport)? {
            if chunk.len() > max.saturating_sub(body.len()) {
                return Err(IndexerError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}
/// Start a cooldown and fail on 429 (or 503 with Retry-After); else return Retry-After.
fn rate_limit(
    gate: &mut Gate,
    response: &reqwest::Response,
) -> Result<Option<Duration>, IndexerError> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(retry_after);
    if status == 429 || retry_after.is_some() && status == 503 {
        let delay = retry_after.unwrap_or(Duration::from_secs(60));
        gate.cooldown = Some((Instant::now(), delay));
        return Err(IndexerError::RateLimited { retry_after: delay });
    }
    Ok(retry_after)
}
/// Cheap check that a body is an NZB document: an `<nzb` root element appears
/// within the prolog window (XML declaration / DOCTYPE allowed before it).
fn looks_like_nzb(body: &[u8]) -> bool {
    let window = &body[..body.len().min(4096)];
    window
        .windows(5)
        .any(|w| w[..4] == *b"<nzb" && matches!(w[4], b' ' | b'>' | b'\t' | b'\r' | b'\n' | b'/'))
}
fn retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|at| at.duration_since(SystemTime::now()).unwrap_or_default())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn url_bearing_transport_error_is_discarded() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let raw = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/api?apikey=never-in-diagnostics"))
            .send()
            .await
            .unwrap_err();
        assert!(raw.url().unwrap().as_str().contains("never-in-diagnostics"));
        let safe = IndexerError::transport(raw);
        assert_eq!(safe, IndexerError::Transport);
        assert!(!format!("{safe:?} {safe}").contains("never-in-diagnostics"));
    }
}
