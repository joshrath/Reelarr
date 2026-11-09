# Contributing to Reelarr

Thanks for your interest! Reelarr is pre-alpha, so expect things to move fast.

## Clean-room rule

Reelarr is written from scratch. Radarr, Sonarr, Prowlarr, Jellyfin, SABnzbd and Seerr are great projects and we study how they *behave*, but **do not copy, translate or port their source code** into Reelarr. Describe behavior in your own words (issues, specs, test cases) and implement it independently. Do not reuse their names, logos or artwork.

Third-party crates are welcome as long as their licenses are compatible with GPL-3.0.

## Workflow

1. Pick or open an issue and say you're working on it.
2. Branch from `main`, keep PRs focused.
3. Before pushing, run:
   ```sh
   cargo fmt --all
   cargo clippy --workspace --all-targets -- -D warnings
   cargo clippy -p reelarr-ui --lib --target wasm32-unknown-unknown --features hydrate -- -D warnings
   cargo test --workspace
   ```
4. By contributing, you agree your work is licensed under GPL-3.0-or-later.

## Web UI (`reelarr-ui`)

The UI is written in Rust with [Leptos](https://leptos.dev). No React, and no JavaScript beyond what Leptos generates (a minimal hls.js loader for the video player will be the one exception).

- `ssr` feature: native build, used by `reelarr-server` to render pages. The crate exposes `reelarr_ui::router(options)`, which the server merges into its axum router.
- `hydrate` feature: `wasm32-unknown-unknown` build that `cargo leptos` turns into `/pkg/reelarr.{js,wasm}` and that hydrates the server-rendered HTML.
- Styles are plain CSS with design tokens in `crates/reelarr-ui/style/main.css`, served as `/pkg/reelarr.css`.
- Routing: `/health` and `/api/v1/*` belong to the server. Leptos server functions, if added, go under `/api/v1/fn/`. Everything else falls back to the UI, except unknown `/api/*` paths, which return a JSON 404.

Run `cargo leptos watch` for a live-reloading dev server.

## Protected API router convention

Merge domain routers into `protected` in `reelarr-server::api::router` **before**
applying `.route_layer(middleware::from_fn_with_state(state, require_auth))`.
Never call `.layer()` on an empty router and merge domain routes afterwards:
Axum layers wrap only routes already present. Keep setup/login/session endpoints
in the public auth router, and merge that router separately. See the fix in
[PR #50](https://github.com/joshrath/Reelarr/pull/50) and the
[auth boundary (#7)](https://github.com/joshrath/Reelarr/issues/7).

The API router's unit regression merges a test-only domain route at this exact
composition point. It requires the middleware-inserted `AuthUser` extension (not
an extractor fallback), asserting anonymous `401` with the `ApiError` JSON body
and authenticated success. Keep this test when adding new domain routers.
