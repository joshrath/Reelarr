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
- Styles are plain CSS in `crates/reelarr-ui/style/main.css`, served as `/pkg/reelarr.css`. Its first section holds the design tokens (colours, type, spacing, radii, the 70s-wood theme and CRT scanlines); components use only those tokens. No Tailwind, no CSS build tools and no CDN links.
- The look follows Josh's design (retro video store and record shop). Its palette names are kept as token names, e.g. `--color-surface-container-high`. Any colour changed for WCAG AA contrast is listed in the token comments.
- Fonts are self-hosted from `crates/reelarr-ui/public/fonts` (served at `/fonts/`), and icons are inline SVG via `components::icon::Icon`. Third-party assets must allow redistribution and be listed in `THIRD_PARTY.md`. Don't commit third-party images or brand logos.
- Routing: `/health` and `/api/v1/*` belong to the server. Leptos server functions, if added, go under `/api/v1/fn/`. Everything else falls back to the UI, except unknown `/api/*` paths, which return a JSON 404.

Run `cargo leptos watch` for a live-reloading dev server.
