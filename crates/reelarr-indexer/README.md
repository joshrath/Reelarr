# reelarr-indexer: Newznab MVP

Clean-room implementation based solely on the [public Newznab API specification](https://newznab.readthedocs.io/en/latest/misc/api.html). Tests/fixtures were written independently for Reelarr. No source from reference applications was consulted or copied.

## Usage / integration

```rust,no_run
use reelarr_indexer::{IndexerConfig, MovieQuery, NewznabClient, SecretString, Url};
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let config = IndexerConfig::new(
    "my indexer", Url::parse("https://indexer.example/")?,
    SecretString::new(std::env::var("INDEXER_API_KEY")?),
);
let client = NewznabClient::new(config)?;
let results = client.search_movie(&MovieQuery {
    title: "An Independent Movie".into(), year: Some(2025),
    imdb_id: Some("tt0000123".into()), ..Default::default()
}).await?;
// Feed results.releases into release parsing / quality selection, not directly into downloads.
// Check results.truncated before claiming the search was exhaustive.
# Ok(()) }
```

- `IndexerStore` is an object-safe async storage interface (upsert/get/list/remove by unique name); `InMemoryIndexerStore` is volatile. The **only persistent DB** remains reelarr-db; it will implement this trait and migrations. No sqlx dependency or private state file.
- #8 passes title/year/IMDb/TMDB through `MovieQuery`; no metadata-provider coupling. #10 receives `Release` candidates and owns title/quality interpretation and ranking. Search never chooses a release.
- Server/UI settings routes, secret-management wiring, persistent adapter and end-to-end movie orchestration are integration work, not implemented here. API keys have **no serde implementation**. This is redaction, not encryption/zeroization. Adapters must use the project's secret-at-rest policy and restrict access. Never expose stored configs/keys through UI DTOs.

## Behavior and limits

- API path is origin-relative, defaults to `/api`. Base URLs may contain a deployment path but the explicit API path is authoritative. Required validation covers empty keys/names, scheme/userinfo/query/fragment, origin changes, category IDs and bounded operational settings.
- Capability discovery sends no key, parses advertised modes/parameters, flattened parent/child categories and default/max limits. One TTL cache is shared by client clones. Explicit invalidation is available; failed refreshes are never cached. Unknown capability extensions are ignored.
- Prefer advertised `imdbid` (strip `tt`, preserve leading zeroes), then advertised `tmdbid`, then advertised movie `q`. If unavailable/empty/203, fall back to advertised general `q` with title + year. Unknown extensions are not sent. Authentication/quota/network/malformed responses are **not** empty results or automatic fallback. Categories, limit and offset use documented base parameters.
- Parse RSS 2.0, enclosures and namespaced Newznab response/attr nodes by **URI**, regardless of prefix. Unknown attributes and repeated values are retained. Optional size/IDs/grabs/files/password/year are typed, pubDate preserves the original RFC 822/2822 text (not a normalized instant), optional missing metadata is allowed. Malformed numbers/required fields fail explicitly.
- Paginate by response offset/total and actual item count, deduplicate GUIDs, reject non-advancing reported offsets, stop on empty/repeated pages and checked-arithmetic overflow. Missing response metadata falls back to page length. Defaults: 20 pages, 1000 unique releases, 4 MiB **per response**, 1000 items requested per page, 100,000 XML nodes / 32 levels. Config hard ceilings: 100 pages, 10,000 results, 16 MiB per body. Search work is bounded by these limits and per-request deadlines; there is no hidden unbounded retry loop.
- XML DTD/entity declarations are rejected; external resources are never resolved. Parse errors do not include raw XML. Response bodies are read incrementally and bounded whether Content-Length is present or absent. Compression is not enabled (limits apply to received uncompressed XML).
- HTTP errors, transport/timeouts and API errors (including HTTP 200 error XML) are typed. Known codes: 100/101/102 credentials/account/privilege, 200/201 parameters, 202/203 unsupported, 300 missing, 910 disabled; other codes remain numeric. Provider **extension** 500 maps to request quota (not claimed as a universal Newznab code). Provider descriptions are parsed but omitted from diagnostics because they can echo credentials.
- One exchange at a time per shared client; configurable minimum request-start interval (default 1s). HTTP 429, 503 with Retry-After and provider quota errors enter a shared cooldown. Retry-After accepts seconds and HTTP dates; default quota delay is 60s. Calls during cooldown return remaining duration rather than sleeping indefinitely or retrying. Higher-level scheduling decides when to retry. No API usage/quota-header budgeting, RSS polling or Torznab yet.

## Transport / secret boundaries

Only rustls is enabled and explicitly selected; no native-tls or OpenSSL linkage. Platform certificate verification remains enabled. Environment/system proxies are disabled to avoid inadvertently routing secrets; redirects are never followed. HTTPS is required by default. `allow_http` is an explicit administrator trust exception, intended for a local/private indexer. Configured origins are **trusted endpoints**: DNS/private-address policy belongs to the settings/admin boundary; do not allow arbitrary remote users to submit indexer URLs. Enclosure URLs must be at the configured origin (including port/scheme), without userinfo/fragment, unless the host is listed in that indexer's `extra_download_hosts` (#44).

### Download hosts and fetching NZBs

Some indexers serve NZBs from a separate download domain. `IndexerConfig::extra_download_hosts` lists exact hostnames trusted for that indexer; it is empty by default, so only the indexer's own origin is accepted. Normalize administrator input with `normalize_download_host` (lowercase/IDNA, no trailing dot); `validate()` rejects unnormalized entries, duplicates, wildcards, ports, paths, userinfo, IP literals, single-label names and more than `MAX_EXTRA_DOWNLOAD_HOSTS` (16) entries. Extra hosts are accepted only over HTTPS on the default port; `allow_http` never extends to them.

`NewznabClient::fetch_nzb(&release)` downloads the NZB body. It applies `IndexerConfig::download_url_allowed` to the enclosure URL and to every redirect `Location`, following at most `MAX_DOWNLOAD_REDIRECTS` (5) hops manually; a hop to a non-allowlisted host or an HTTP downgrade fails with `UnsafeDownloadUrl`, and too many hops with `TooManyRedirects`. It shares the indexer's rate gate, cooldown, request timeout and `max_response_bytes` cap, maps Newznab error documents to typed errors, and makes only a light `<nzb` sanity check (`InvalidNzb`); full NZB parsing belongs to reelarr-nzb. As everywhere in this crate, errors and `Debug` output never contain URLs, queries or the API key.

Persistence of `extra_download_hosts` is the storage adapter's job (reelarr-db).

`SecretString` always redacts Debug/Display; API keys are added only at send time. Config Debug redacts even invalid URL/name/path fields. Release Debug omits all raw provider strings, including GUIDs that may themselves be URLs. Transport errors discard the underlying reqwest error rather than retaining a URL-bearing source. No request URL, response body or key is emitted via tracing. Exposed URL strings, provider attributes and titles remain untrusted; consumers must not log them. HTTP access logging and external proxies are outside this crate's boundary.

## Verification / licenses

`cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test --workspace`.

Direct dependencies: existing tokio 1.54.1 (MIT); reqwest 0.13.5, roxmltree 0.21.2, httpdate 1.0.3 (each MIT OR Apache-2.0); test-only wiremock 0.6.5 (MIT OR Apache-2.0). Selected release manifest declarations and MIT license texts were read. roxmltree was chosen for simple namespace-aware parsing of an already bounded body; the NZB parser's streaming XML decision is separate.

See [DEPENDENCIES.md](DEPENDENCIES.md) for new lockfile-package license declarations and TLS provenance. These permissive licenses are compatible with the GPL-3.0-or-later workspace. Release packaging must retain required notices/license texts and produce a full artifact SBOM; this source-only change does not claim a legal review or full provenance audit.
