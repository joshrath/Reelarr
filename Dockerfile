# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
ARG CARGO_LEPTOS_VERSION=0.3.11
RUN rustup target add wasm32-unknown-unknown \
    && curl -fsSL "https://github.com/leptos-rs/cargo-leptos/releases/download/v${CARGO_LEPTOS_VERSION}/cargo-leptos-x86_64-unknown-linux-gnu.tar.gz" \
       | tar -xz -C /tmp \
    && install /tmp/cargo-leptos-x86_64-unknown-linux-gnu/cargo-leptos /usr/local/cargo/bin/
WORKDIR /src
COPY . .
# Builds the server binary and the web UI (WASM, JS and CSS in target/site).
RUN cargo leptos build --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/reelarr /usr/local/bin/reelarr
COPY --from=builder /src/target/site /usr/share/reelarr/site
ENV REELARR_PORT=7979 \
    LEPTOS_SITE_ROOT=/usr/share/reelarr/site \
    LEPTOS_ENV=PROD
VOLUME ["/config", "/media", "/downloads"]
EXPOSE 7979
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["reelarr"]
