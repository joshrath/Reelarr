//! The Record Corner: music. Music libraries aren't supported yet, so this
//! is the themed aisle with an honest empty crate and a pointer to the
//! library folders. No API calls, no sample records. Copy lives in
//! [`crate::records`].

use leptos::prelude::*;
use leptos_meta::Title;

use crate::components::icon::{Icon, IconName};
use crate::records::{
    EMPTY_LINK_LABEL, EMPTY_MESSAGE, EMPTY_TITLE, LEDE, NOTE, OVERLINE, SETTINGS_PATH,
    STATUS_LABEL, STATUS_VALUE, SUBTITLE, TITLE,
};

#[component]
pub fn RecordCornerPage() -> impl IntoView {
    view! {
        <Title text=TITLE />
        <section class="page records" aria-labelledby="page-title">
            <header class="records-head">
                <div class="records-head-title">
                    <p class="records-overline label">
                        <Icon name=IconName::Speaker />
                        {OVERLINE}
                    </p>
                    <h1 id="page-title">{TITLE}</h1>
                    <p class="records-subtitle">
                        <span class="records-slash" aria-hidden="true">"// "</span>
                        {SUBTITLE}
                    </p>
                    <p class="records-lede">{LEDE}</p>
                </div>
                <div class="records-status">
                    <p class="records-status-label label">{STATUS_LABEL}</p>
                    <p class="records-status-value">{STATUS_VALUE}</p>
                </div>
            </header>

            <section class="records-crate" aria-labelledby="records-crate-title">
                <span class="records-crate-icon" aria-hidden="true">
                    <Icon name=IconName::Radio />
                </span>
                <h2 id="records-crate-title" class="records-crate-title">{EMPTY_TITLE}</h2>
                <p class="records-crate-message">{EMPTY_MESSAGE}</p>
                <p class="records-crate-actions">
                    <a class="button primary" href=SETTINGS_PATH>
                        <Icon name=IconName::HardDrive />
                        {EMPTY_LINK_LABEL}
                    </a>
                </p>
            </section>

            <p class="records-note">
                <Icon name=IconName::Info />
                {NOTE}
            </p>
        </section>
    }
}
