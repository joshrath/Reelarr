//! Display preferences: the colour theme and the CRT scanline overlay.
//!
//! Both are stored in cookies so the server renders the page with them
//! already applied, without a flash of the default look. Changing one updates
//! the `<html>` element directly and rewrites its cookie.

use leptos::prelude::*;

pub const THEME_COOKIE: &str = "reelarr_theme";
pub const CRT_COOKIE: &str = "reelarr_crt";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    /// '80s VHS neon, the default.
    #[default]
    Vhs80s,
    /// '70s wood paneling.
    Wood70s,
}

impl Theme {
    /// Value stored in the cookie and on `<html data-theme>`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vhs80s => "80s-vhs",
            Self::Wood70s => "70s-wood",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "70s-wood" => Self::Wood70s,
            _ => Self::Vhs80s,
        }
    }
}

/// Saved display preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Prefs {
    pub theme: Theme,
    pub crt: bool,
}

impl Prefs {
    /// Reads preferences from a `Cookie` header value. Unknown or missing
    /// values fall back to the defaults.
    pub fn from_cookie_header(header: &str) -> Self {
        let mut prefs = Self::default();
        for pair in header.split(';') {
            let Some((name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            match name {
                THEME_COOKIE => prefs.theme = Theme::parse(value),
                CRT_COOKIE => prefs.crt = value == "on",
                _ => {}
            }
        }
        prefs
    }

    /// Preferences for the current request (server) or page (browser).
    pub fn current() -> Self {
        #[cfg(feature = "ssr")]
        {
            use_context::<axum::http::request::Parts>()
                .and_then(|parts| {
                    parts
                        .headers
                        .get_all(axum::http::header::COOKIE)
                        .iter()
                        .filter_map(|v| v.to_str().ok())
                        .map(Self::from_cookie_header)
                        .reduce(|a, b| Self {
                            theme: if b.theme != Theme::default() {
                                b.theme
                            } else {
                                a.theme
                            },
                            crt: a.crt || b.crt,
                        })
                })
                .unwrap_or_default()
        }
        #[cfg(not(feature = "ssr"))]
        {
            document()
                .document_element()
                .map(|html| Self {
                    theme: Theme::parse(&html.get_attribute("data-theme").unwrap_or_default()),
                    crt: html.class_list().contains("crt"),
                })
                .unwrap_or_default()
        }
    }

    /// `class` for the `<html>` element.
    pub fn html_class(self) -> &'static str {
        if self.crt { "crt" } else { "" }
    }
}

/// Reactive preferences shared through context.
#[derive(Clone, Copy)]
pub struct PrefsSignal(pub RwSignal<Prefs>);

impl PrefsSignal {
    pub fn provide() -> Self {
        let signal = Self(RwSignal::new(Prefs::current()));
        provide_context(signal);
        signal
    }

    pub fn use_context() -> Self {
        expect_context::<Self>()
    }

    pub fn set_theme(self, theme: Theme) {
        self.0.update(|p| p.theme = theme);
        self.persist();
    }

    pub fn toggle_crt(self) {
        self.0.update(|p| p.crt = !p.crt);
        self.persist();
    }

    /// Applies the preferences to `<html>` and saves them for a year.
    fn persist(self) {
        #[cfg(not(feature = "ssr"))]
        {
            use web_sys::wasm_bindgen::JsCast;
            let prefs = self.0.get_untracked();
            let doc = document();
            if let Some(html) = doc.document_element() {
                let _ = html.set_attribute("data-theme", prefs.theme.as_str());
                let _ = html.class_list().toggle_with_force("crt", prefs.crt);
            }
            if let Ok(doc) = doc.dyn_into::<web_sys::HtmlDocument>() {
                let attrs = "; Path=/; Max-Age=31536000; SameSite=Lax";
                let _ = doc.set_cookie(&format!("{THEME_COOKIE}={}{attrs}", prefs.theme.as_str()));
                let crt = if prefs.crt { "on" } else { "off" };
                let _ = doc.set_cookie(&format!("{CRT_COOKIE}={crt}{attrs}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_header() {
        let prefs = Prefs::from_cookie_header("a=b; reelarr_theme=70s-wood; reelarr_crt=on");
        assert_eq!(
            prefs,
            Prefs {
                theme: Theme::Wood70s,
                crt: true
            }
        );
    }

    #[test]
    fn unknown_values_fall_back_to_defaults() {
        let prefs = Prefs::from_cookie_header("reelarr_theme=neon; reelarr_crt=yes; junk");
        assert_eq!(prefs, Prefs::default());
        assert_eq!(Prefs::from_cookie_header(""), Prefs::default());
    }

    #[test]
    fn theme_round_trips() {
        for theme in [Theme::Vhs80s, Theme::Wood70s] {
            assert_eq!(Theme::parse(theme.as_str()), theme);
        }
    }
}
