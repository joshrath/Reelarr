# Third-party assets

Assets shipped with Reelarr that were not written for it. Each one must allow
redistribution under terms compatible with GPL-3.0. Code dependencies are
tracked in `Cargo.lock`; this file covers fonts, icons and other files
committed to the repository.

| Asset | Where | Source | License |
|---|---|---|---|
| Bebas Neue (Regular), Latin subset as woff2 | `crates/reelarr-ui/public/fonts/bebas-neue-400.woff2` | [google/fonts `ofl/bebasneue`](https://github.com/google/fonts/tree/main/ofl/bebasneue), © 2010 Dharma Type | SIL Open Font License 1.1, text in `OFL-bebasneue.txt` |
| Plus Jakarta Sans (variable weight), Latin subset as woff2 | `crates/reelarr-ui/public/fonts/plus-jakarta-sans-var.woff2` | [google/fonts `ofl/plusjakartasans`](https://github.com/google/fonts/tree/main/ofl/plusjakartasans), © 2020 The Plus Jakarta Sans Project Authors | SIL Open Font License 1.1, text in `OFL-plusjakartasans.txt` |
| Space Mono (Regular, Bold, Italic), Latin subset as woff2 | `crates/reelarr-ui/public/fonts/space-mono-*.woff2` | [google/fonts `ofl/spacemono`](https://github.com/google/fonts/tree/main/ofl/spacemono), © 2016 The Space Mono Project Authors | SIL Open Font License 1.1, text in `OFL-spacemono.txt` |
| Material Symbols (Outlined, 24px), SVG path data for 44 icons | `crates/reelarr-ui/src/components/icon.rs` | [google/material-design-icons](https://github.com/google/material-design-icons) | Apache License 2.0 |

None of these fonts declares a Reserved Font Name, so the subsets keep their
original family names. Subsets were made with `pyftsubset` (fontTools) using
the Basic Latin and Latin-1 ranges plus common punctuation and arrows, and the
fonts are otherwise unmodified.
