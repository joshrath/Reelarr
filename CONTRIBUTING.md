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
   cargo test --workspace
   ```
4. By contributing, you agree your work is licensed under GPL-3.0-or-later.
