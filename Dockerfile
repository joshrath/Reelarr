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
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates tini util-linux \
    && rm -f /usr/bin/ffplay /usr/bin/qt-faststart \
    && mkdir -p /config /usr/share/reelarr/licenses \
    && chown 1000:1000 /config \
    && rm -rf /var/lib/apt/lists/*
COPY docker/check-runtime.sh /usr/local/share/reelarr/check-runtime.sh
COPY docs/licenses.md /usr/share/reelarr/licenses/README.md
COPY LICENSE /usr/share/reelarr/licenses/Reelarr-LICENSE
RUN /usr/local/share/reelarr/check-runtime.sh \
    && dpkg-query -W > /usr/share/reelarr/licenses/packages.tsv \
    && ffmpeg -version > /usr/share/reelarr/licenses/ffmpeg-version.txt 2>&1 \
    && ffmpeg -buildconf > /usr/share/reelarr/licenses/ffmpeg-buildconf.txt 2>&1 \
    && ffmpeg -L > /usr/share/reelarr/licenses/ffmpeg-license.txt 2>&1 \
    && tini --version > /usr/share/reelarr/licenses/tini-version.txt 2>&1
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/reelarr-entrypoint
COPY --from=builder /src/target/release/reelarr /usr/local/bin/reelarr
COPY --from=builder /src/target/site /usr/share/reelarr/site
ENV PUID=1000 \
    PGID=1000 \
    REELARR_PORT=7979 \
    LEPTOS_SITE_ROOT=/usr/share/reelarr/site \
    LEPTOS_ENV=PROD
VOLUME ["/config", "/media", "/downloads"]
EXPOSE 7979
USER 1000:1000
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/reelarr-entrypoint", "/usr/local/bin/reelarr", "healthcheck"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/reelarr-entrypoint"]
CMD ["reelarr"]
