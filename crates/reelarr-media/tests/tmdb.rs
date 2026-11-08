//! TMDB provider and movie service tests against a mock HTTP server.
//! No live network calls are made.

use reelarr_media::metadata::tmdb::{TmdbConfig, TmdbCredential, TmdbProvider};
use reelarr_media::{
    AddMovieError, InMemoryMovieStore, MetadataError, MetadataProvider, MovieService, MovieStore,
    SearchQuery,
};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SEARCH: &str = include_str!("fixtures/search_movie.json");
const DETAILS: &str = include_str!("fixtures/movie_details.json");
const DETAILS_SPARSE: &str = include_str!("fixtures/movie_details_sparse.json");
const TOKEN: &str = "eyJtest.token.value";

fn provider(server: &MockServer, credential: TmdbCredential) -> TmdbProvider {
    let mut config = TmdbConfig::new(credential);
    config.api_base = server.uri();
    TmdbProvider::new(config).unwrap()
}

fn json(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

#[tokio::test]
async fn search_maps_results_and_skips_unusable_ones() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/search/movie"))
        .and(query_param("query", "Example Movie"))
        .and(query_param("year", "2004"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(json(SEARCH))
        .expect(1)
        .mount(&server)
        .await;

    let tmdb = provider(&server, TmdbCredential::detect(TOKEN));
    let results = tmdb
        .search_movies(&SearchQuery::new("Example Movie").with_year(2004))
        .await
        .unwrap();

    assert_eq!(results.len(), 2, "result with empty title is skipped");
    assert_eq!(results[0].tmdb_id, 1001);
    assert_eq!(results[0].year, Some(2004));
    assert_eq!(results[0].poster_path.as_deref(), Some("/poster-one.jpg"));
    assert_eq!(results[1].year, None, "empty release date gives no year");
    assert_eq!(results[1].overview, None, "empty overview becomes None");
    assert_eq!(results[1].poster_path, None);
}

#[tokio::test]
async fn api_key_is_sent_as_query_parameter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/3/movie/1001"))
        .and(query_param("api_key", "v3key"))
        .respond_with(json(DETAILS))
        .expect(1)
        .mount(&server)
        .await;

    let tmdb = provider(&server, TmdbCredential::detect("v3key"));
    let movie = tmdb.movie_details(1001).await.unwrap();
    assert_eq!(movie.title, "The Example Movie");
    assert_eq!(movie.external_ids.imdb_id.as_deref(), Some("tt0000001"));
    assert_eq!(movie.year, Some(2004));
    assert_eq!(movie.runtime_minutes, Some(112));
    assert_eq!(
        movie.artwork.backdrop_path.as_deref(),
        Some("/backdrop-one.jpg")
    );
}

#[tokio::test]
async fn sparse_details_map_missing_fields_to_none() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/1002"))
        .respond_with(json(DETAILS_SPARSE))
        .mount(&server)
        .await;

    let movie = provider(&server, TmdbCredential::detect(TOKEN))
        .movie_details(1002)
        .await
        .unwrap();
    assert_eq!(movie.external_ids.imdb_id, None);
    assert_eq!(movie.year, None);
    assert_eq!(movie.overview, None);
    assert_eq!(movie.runtime_minutes, None);
    assert_eq!(movie.artwork.poster_path, None);
    assert_eq!(movie.original_title.as_deref(), Some("Le Film Exemple"));
}

#[tokio::test]
async fn http_errors_map_to_typed_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/1"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/2"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/3"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "7"))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/4"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/5"))
        .respond_with(json("{not json"))
        .mount(&server)
        .await;
    Mock::given(path("/3/movie/6"))
        .respond_with(json(r#"{"id": 6}"#))
        .mount(&server)
        .await;

    let tmdb = provider(&server, TmdbCredential::detect(TOKEN));
    assert!(matches!(
        tmdb.movie_details(1).await,
        Err(MetadataError::Unauthorized)
    ));
    assert!(matches!(
        tmdb.movie_details(2).await,
        Err(MetadataError::NotFound(2))
    ));
    assert!(matches!(
        tmdb.movie_details(3).await,
        Err(MetadataError::RateLimited {
            retry_after_secs: Some(7)
        })
    ));
    assert!(matches!(
        tmdb.movie_details(4).await,
        Err(MetadataError::Http(503))
    ));
    assert!(matches!(
        tmdb.movie_details(5).await,
        Err(MetadataError::Decode(_))
    ));
    assert!(matches!(
        tmdb.movie_details(6).await,
        Err(MetadataError::MissingField("title"))
    ));
}

#[tokio::test]
async fn empty_search_is_rejected_without_a_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(json(SEARCH))
        .expect(0)
        .mount(&server)
        .await;
    let tmdb = provider(&server, TmdbCredential::detect(TOKEN));
    assert!(matches!(
        tmdb.search_movies(&SearchQuery::new("   ")).await,
        Err(MetadataError::InvalidQuery(_))
    ));
}

#[tokio::test]
async fn add_movie_persists_and_rejects_duplicates() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/1001"))
        .respond_with(json(DETAILS))
        .expect(1) // the duplicate add must not hit the API again
        .mount(&server)
        .await;

    let service = MovieService::new(
        provider(&server, TmdbCredential::detect(TOKEN)),
        InMemoryMovieStore::new(),
    );

    let movie = service.add_movie(1001).await.unwrap();
    assert_eq!(movie.metadata.external_ids.tmdb_id, 1001);
    assert!(movie.metadata_fetched_at > 0);

    let stored = service.store().get(movie.id).await.unwrap().unwrap();
    assert_eq!(stored, movie);
    assert_eq!(service.store().list().await.unwrap().len(), 1);

    match service.add_movie(1001).await {
        Err(AddMovieError::AlreadyExists(id)) => assert_eq!(id, movie.id),
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
}

#[tokio::test]
async fn add_movie_surfaces_not_found_and_stores_nothing() {
    let server = MockServer::start().await;
    Mock::given(path("/3/movie/999"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let service = MovieService::new(
        provider(&server, TmdbCredential::detect(TOKEN)),
        InMemoryMovieStore::new(),
    );
    assert!(matches!(
        service.add_movie(999).await,
        Err(AddMovieError::Metadata(MetadataError::NotFound(999)))
    ));
    assert!(service.store().list().await.unwrap().is_empty());
}
