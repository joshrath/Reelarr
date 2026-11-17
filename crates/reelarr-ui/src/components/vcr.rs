//! The VCR player from the design: a screen with an on-screen display, and
//! a control deck with a tape counter, a scrubber and hardware-style
//! transport keys.
//!
//! Playback is the browser's own `<video>` element driven through web-sys.
//! Its native controls are on until the custom deck is bound in the
//! browser, so the tape stays playable if that ever fails.

use std::time::Duration;

use leptos::{ev, html, prelude::*};
use leptos_router::hooks::use_navigate;
use web_sys::wasm_bindgen::JsCast;

use super::icon::{Icon, IconName};
use reelarr_types::PlaybackInfo;

use crate::api::get_json;
use crate::playback::{
    DeckState, Notice, SIGN_IN_PATH, format_counter, notice_after_media_error, playback_path,
};

/// Seconds the REW and FF keys jump.
pub const SKIP_SECONDS: f64 = 30.0;
/// Seconds the arrow keys jump.
pub const NUDGE_SECONDS: f64 = 10.0;
/// Where EJECT goes: the Store Floor.
pub const EJECT_PATH: &str = "/";
/// How long the REW/FF reading stays on the on-screen display when no
/// `seeked` or `loadedmetadata` event clears it first.
pub const FLASH_MS: u64 = 1200;

/// The screen after the `<video>` reports a `MediaError`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stopped {
    /// Fetching the playback answer again to find out why.
    Checking,
    Show(Notice),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Stopped,
    Playing,
    Paused,
}

/// The full player for one deck state. Transport keys other than EJECT are
/// disabled while there is no playable tape.
#[component]
pub fn VcrPlayer(
    state: DeckState,
    /// The media file on the deck, used to re-fetch its playback answer
    /// when the `<video>` reports an error.
    #[prop(default = None)]
    media_id: Option<i64>,
    /// Shown as a "Try again" button for errors worth retrying.
    #[prop(optional, into)]
    on_retry: Option<Callback<()>>,
) -> AnyView {
    let loading = state == DeckState::Loading;
    let (stream_url, duration_ms, notice) = match state {
        DeckState::Loading => (None, None, None),
        DeckState::Ready {
            stream_url,
            duration_ms,
        } => (Some(stream_url), duration_ms, None),
        DeckState::Notice(notice) => (None, None, Some(notice)),
    };
    let has_tape = stream_url.is_some();

    let video_ref = NodeRef::<html::Video>::new();
    let mode = RwSignal::new(Mode::Stopped);
    // Brief OSD reading for a seek, cleared once the seek lands, the
    // metadata loads, or after FLASH_MS, whichever comes first.
    let flash = RwSignal::new(None::<&'static str>);
    let flash_generation = StoredValue::new(0_u32);
    let current = RwSignal::new(0.0_f64);
    // True while the scrubber is held, so `timeupdate` doesn't move it
    // out from under the pointer.
    let scrubbing = RwSignal::new(false);
    let duration = RwSignal::new(duration_ms.map_or(f64::NAN, |ms| ms as f64 / 1000.0));
    let volume = RwSignal::new(1.0_f64);
    let muted = RwSignal::new(false);
    let crt = RwSignal::new(true);
    let media_error = RwSignal::new(None::<Stopped>);
    // Bumped on every error and every play, so a stale re-fetch is ignored.
    let error_generation = StoredValue::new(0_u32);
    // True once the custom deck is bound to the video in the browser.
    let enhanced = RwSignal::new(false);

    // Browser only; skipped on the server even when a build turns on both
    // `ssr` and `hydrate` (e.g. `--all-features`).
    #[cfg(not(feature = "ssr"))]
    Effect::new(move |_| {
        if video_ref.get().is_some() {
            enhanced.set(true);
        }
    });
    on_cleanup(move || {
        // Stop the range requests when leaving the page.
        if let Some(video) = video_ref.get_untracked() {
            let _ = video.pause();
            let _ = video.remove_attribute("src");
            video.load();
        }
    });

    let toggle = move || {
        let Some(video) = video_ref.get_untracked() else {
            return;
        };
        if video.paused() {
            if let Ok(promise) = video.play() {
                // A refused play (e.g. a load error) is reported by `error`.
                leptos::task::spawn_local(async move {
                    let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                });
            }
        } else {
            let _ = video.pause();
        }
    };
    let seek_to = move |seconds: f64| {
        let Some(video) = video_ref.get_untracked() else {
            return;
        };
        let end = video.duration();
        let target = if end.is_finite() {
            seconds.clamp(0.0, end)
        } else {
            seconds.max(0.0)
        };
        video.set_current_time(target);
        current.set(target);
    };
    let show_flash = move |reading: &'static str| {
        flash.set(Some(reading));
        let generation = flash_generation.get_value().wrapping_add(1);
        flash_generation.set_value(generation);
        // The player may be gone by the time this fires.
        set_timeout(
            move || {
                if flash_generation.try_get_value() == Some(generation) {
                    flash.try_set(None);
                }
            },
            Duration::from_millis(FLASH_MS),
        );
    };
    let seek_by = move |delta: f64| {
        if let Some(video) = video_ref.get_untracked() {
            show_flash(if delta < 0.0 {
                "REW ◀◀"
            } else {
                "FF ▶▶"
            });
            seek_to(video.current_time() + delta);
        }
    };
    let stop = move || {
        if let Some(video) = video_ref.get_untracked() {
            mode.set(Mode::Stopped);
            let _ = video.pause();
            video.set_current_time(0.0);
            current.set(0.0);
        }
    };
    let set_volume = move |level: f64| {
        if let Some(video) = video_ref.get_untracked() {
            let level = level.clamp(0.0, 1.0);
            video.set_volume(level);
            if level > 0.0 {
                video.set_muted(false);
            }
        }
    };
    let toggle_mute = move || {
        if let Some(video) = video_ref.get_untracked() {
            video.set_muted(!video.muted());
        }
    };
    let navigate = use_navigate();
    let eject = move |_| {
        stop();
        navigate(EJECT_PATH, Default::default());
    };

    // Keyboard: Space/K play or pause, arrows seek and set the volume,
    // M mutes. Sliders and buttons keep their own keys.
    let on_keydown = move |e: ev::KeyboardEvent| {
        if !has_tape || e.ctrl_key() || e.meta_key() || e.alt_key() {
            return;
        }
        let tag = e
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .map(|el| el.tag_name())
            .unwrap_or_default();
        if tag == "INPUT" {
            return;
        }
        let on_control = tag == "BUTTON" || tag == "A";
        match e.key().as_str() {
            " " if on_control => return,
            " " | "k" | "K" => toggle(),
            "ArrowLeft" | "j" | "J" => seek_by(-NUDGE_SECONDS),
            "ArrowRight" | "l" | "L" => seek_by(NUDGE_SECONDS),
            "ArrowUp" => set_volume(volume.get_untracked() + 0.1),
            "ArrowDown" => set_volume(volume.get_untracked() - 0.1),
            "m" | "M" => toggle_mute(),
            "Home" => seek_to(0.0),
            _ => return,
        }
        e.prevent_default();
    };

    let osd_mode = move || {
        if loading {
            return "LOADING";
        }
        if !has_tape {
            return "NO TAPE";
        }
        flash.get().unwrap_or(match mode.get() {
            Mode::Playing => "PLAY ▶",
            Mode::Paused => "PAUSE ❚❚",
            Mode::Stopped => "STOP ■",
        })
    };
    let remaining = move || format_counter(duration.get() - current.get());
    let playing = move || mode.get() == Mode::Playing;

    let screen = match stream_url {
        Some(url) => view! {
            <video
                node_ref=video_ref
                class="vcr-video"
                src=url
                preload="metadata"
                playsinline=true
                controls=move || !enhanced.get()
                on:click=move |_| {
                    if enhanced.get_untracked() {
                        toggle();
                    }
                }
                on:play=move |_| {
                    mode.set(Mode::Playing);
                    error_generation.update_value(|g| *g = g.wrapping_add(1));
                    media_error.set(None);
                }
                on:pause=move |_| {
                    mode.update(|m| {
                        if *m != Mode::Stopped {
                            *m = Mode::Paused;
                        }
                    })
                }
                on:ended=move |_| mode.set(Mode::Stopped)
                on:seeked=move |_| flash.set(None)
                on:loadedmetadata=move |_| flash.set(None)
                on:timeupdate=move |_| {
                    if scrubbing.get_untracked() {
                        return;
                    }
                    if let Some(video) = video_ref.get_untracked() {
                        current.set(video.current_time());
                    }
                }
                on:durationchange=move |_| {
                    if let Some(video) = video_ref.get_untracked() {
                        let end = video.duration();
                        if end.is_finite() {
                            duration.set(end);
                        }
                    }
                }
                on:volumechange=move |_| {
                    if let Some(video) = video_ref.get_untracked() {
                        volume.set(video.volume());
                        muted.set(video.muted());
                    }
                }
                on:error=move |_| {
                    let code = video_ref
                        .get_untracked()
                        .and_then(|video| video.error())
                        .map_or(0, |error| error.code());
                    mode.set(Mode::Stopped);
                    flash.set(None);
                    let Some(id) = media_id else {
                        media_error.set(Some(Stopped::Show(Notice::Stopped(code))));
                        return;
                    };
                    // Ask the server again: only a 401 means the session
                    // ended; anything else gets its usual notice.
                    let generation = error_generation.get_value().wrapping_add(1);
                    error_generation.set_value(generation);
                    media_error.set(Some(Stopped::Checking));
                    leptos::task::spawn_local(async move {
                        let recheck = get_json::<PlaybackInfo>(&playback_path(id)).await;
                        if error_generation.try_get_value() == Some(generation) {
                            media_error
                                .try_set(Some(Stopped::Show(notice_after_media_error(code, recheck))));
                        }
                    });
                }
            ></video>
            {move || {
                media_error
                    .get()
                    .map(|stopped| match stopped {
                        Stopped::Checking => view! {
                            <div class="vcr-notice vcr-notice-quiet" role="status">
                                <p class="vcr-notice-title">"Playback stopped"</p>
                                <p>"Checking the tape…"</p>
                            </div>
                        }
                        .into_any(),
                        Stopped::Show(notice) => view! { <NoticeCard notice on_retry /> }.into_any(),
                    })
            }}
        }
        .into_any(),
        None => match notice {
            Some(notice) => view! { <NoticeCard notice on_retry /> }.into_any(),
            None => view! {
                <div class="vcr-notice vcr-notice-quiet" role="status">
                    <p class="vcr-notice-title">"Loading tape…"</p>
                </div>
            }
            .into_any(),
        },
    };

    view! {
        <section class="vcr" aria-label="VCR player" on:keydown=on_keydown>
            <div
                class="vcr-screen"
                class:vcr-crt=move || crt.get()
                tabindex=has_tape.then_some("0")
                aria-label=has_tape.then_some("Screen")
                aria-describedby=has_tape.then_some("vcr-keys")
            >
                {screen}
                <div class="vcr-osd">
                    <div class="vcr-osd-left">
                        <p class="vcr-osd-mode">
                            <span role="status">{osd_mode}</span>
                            <span class="vcr-osd-speed" aria-hidden="true">"SP"</span>
                        </p>
                    </div>
                    <p class="vcr-osd-chip" aria-hidden="true">
                        <span class="vcr-osd-label">"REMAIN:"</span>
                        <span>{remaining}</span>
                    </p>
                </div>
            </div>
            <p id="vcr-keys" class="visually-hidden">
                "Space or K plays and pauses. Left and right arrows rewind and fast-forward 10 seconds. Up and down arrows change the volume. M mutes."
            </p>
            <div class="vcr-deck">
                <p class="vcr-counter">
                    <span class="visually-hidden">"Tape position "</span>
                    <span class="vcr-counter-now">
                        {move || {
                            if has_tape { format_counter(current.get()) } else { format_counter(f64::NAN) }
                        }}
                    </span>
                    <span class="vcr-counter-sep" aria-hidden="true">"/"</span>
                    <span class="visually-hidden">" of "</span>
                    <span class="vcr-counter-total">{move || format_counter(duration.get())}</span>
                </p>
                <input
                    type="range"
                    class="vcr-scrubber"
                    aria-label="Seek"
                    min="0"
                    step="1"
                    prop:max=move || {
                        let end = duration.get();
                        if end.is_finite() { end.floor() } else { 0.0 }
                    }
                    prop:value=move || current.get().floor().to_string()
                    aria-valuetext=move || {
                        format!("{} of {}", format_counter(current.get()), format_counter(duration.get()))
                    }
                    style:--vcr-progress=move || {
                        let end = duration.get();
                        let pct = if end.is_finite() && end > 0.0 {
                            (current.get() / end * 100.0).clamp(0.0, 100.0)
                        } else {
                            0.0
                        };
                        format!("{pct:.2}%")
                    }
                    disabled=!has_tape
                    on:pointerdown=move |_| scrubbing.set(true)
                    on:pointerup=move |_| scrubbing.set(false)
                    on:pointercancel=move |_| scrubbing.set(false)
                    on:change=move |_| scrubbing.set(false)
                    on:blur=move |_| scrubbing.set(false)
                    on:input=move |e| {
                        if let Ok(seconds) = event_target_value(&e).parse::<f64>() {
                            seek_to(seconds);
                        }
                    }
                />
                <div class="vcr-controls">
                    <div class="vcr-transport" role="group" aria-label="Transport">
                        <button
                            type="button"
                            class="vcr-key"
                            disabled=!has_tape
                            on:click=move |_| seek_by(-SKIP_SECONDS)
                        >
                            <Icon name=IconName::FastRewind />
                            "REW"
                            <span class="visually-hidden">" (rewind 30 seconds)"</span>
                        </button>
                        <button
                            type="button"
                            class="vcr-key vcr-key-play"
                            disabled=!has_tape
                            on:click=move |_| toggle()
                        >
                            {move || {
                                if playing() {
                                    view! { <Icon name=IconName::Pause /> "PAUSE" }.into_any()
                                } else {
                                    view! { <Icon name=IconName::PlayArrow /> "PLAY" }.into_any()
                                }
                            }}
                        </button>
                        <button
                            type="button"
                            class="vcr-key"
                            disabled=!has_tape
                            on:click=move |_| stop()
                        >
                            <Icon name=IconName::Stop />
                            "STOP"
                        </button>
                        <button
                            type="button"
                            class="vcr-key"
                            disabled=!has_tape
                            on:click=move |_| seek_by(SKIP_SECONDS)
                        >
                            <Icon name=IconName::FastForward />
                            "FF"
                            <span class="visually-hidden">" (fast forward 30 seconds)"</span>
                        </button>
                        <button type="button" class="vcr-key vcr-key-eject" on:click=eject>
                            <Icon name=IconName::Eject />
                            "EJECT"
                        </button>
                    </div>
                    <div class="vcr-aux">
                        <div class="vcr-volume">
                            <button
                                type="button"
                                class="vcr-key vcr-key-small"
                                aria-pressed=move || muted.get().to_string()
                                disabled=!has_tape
                                on:click=move |_| toggle_mute()
                            >
                                <Icon name=IconName::VolumeUp />
                                {move || if muted.get() { "MUTED" } else { "MUTE" }}
                            </button>
                            <input
                                type="range"
                                class="vcr-volume-slider"
                                aria-label="Volume"
                                min="0"
                                max="100"
                                step="5"
                                prop:value=move || {
                                    let level = if muted.get() { 0.0 } else { volume.get() };
                                    (level * 100.0).round().to_string()
                                }
                                style:--vcr-progress=move || {
                                    let level = if muted.get() { 0.0 } else { volume.get() };
                                    format!("{:.0}%", level * 100.0)
                                }
                                disabled=!has_tape
                                on:input=move |e| {
                                    if let Ok(level) = event_target_value(&e).parse::<f64>() {
                                        set_volume(level / 100.0);
                                    }
                                }
                            />
                        </div>
                        <button
                            type="button"
                            class="vcr-key vcr-key-small vcr-crt-toggle"
                            aria-pressed=move || crt.get().to_string()
                            on:click=move |_| crt.update(|on| *on = !*on)
                        >
                            <Icon name=IconName::Tv />
                            {move || if crt.get() { "CRT: ON" } else { "CRT: OFF" }}
                        </button>
                    </div>
                </div>
            </div>
        </section>
    }
    .into_any()
}

/// A message on the screen in place of the picture.
#[component]
fn NoticeCard(notice: Notice, on_retry: Option<Callback<()>>) -> AnyView {
    let action = match &notice {
        Notice::SignIn => {
            Some(view! { <a class="button primary" href=SIGN_IN_PATH>"Sign in"</a> }.into_any())
        }
        Notice::TryAgain | Notice::Stopped(_) => on_retry.map(|retry| {
            view! {
                <button type="button" class="button primary" on:click=move |_| retry.run(())>
                    "Try again"
                </button>
            }
            .into_any()
        }),
        _ => None,
    };
    let details = notice.details().to_vec();
    view! {
        <div class="vcr-notice" role="alert">
            <p class="vcr-notice-title">{notice.title()}</p>
            <p>{notice.message()}</p>
            {(!details.is_empty())
                .then(|| {
                    view! {
                        <ul class="vcr-notice-details">
                            {details.into_iter().map(|d| view! { <li>{d}</li> }).collect_view()}
                        </ul>
                    }
                })}
            <p class="vcr-notice-actions">
                {action}
                <a class="button" href=EJECT_PATH>"Back to the Store Floor"</a>
            </p>
        </div>
    }
    .into_any()
}
