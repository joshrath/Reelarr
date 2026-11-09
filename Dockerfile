# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
ARG TARGETARCH
ARG CARGO_LEPTOS_VERSION=0.3.11
# sha256 of the cargo-leptos release tarballs for CARGO_LEPTOS_VERSION.
ARG CARGO_LEPTOS_SHA256_X86_64=1c9cf0e1e3ffae6c8b58d29281d949dab560cea613b80d361f323f93071df7c8
ARG CARGO_LEPTOS_SHA256_AARCH64=8aa36d841d69fb62fa00a3e0d8670a52dc16a9a94c2c6f96c545d889c49f18aa
RUN set -eu; \
    case "${TARGETARCH:-amd64}" in \
      amd64) arch=x86_64; sum="$CARGO_LEPTOS_SHA256_X86_64" ;; \
      arm64) arch=aarch64; sum="$CARGO_LEPTOS_SHA256_AARCH64" ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    rustup target add wasm32-unknown-unknown; \
    curl -fsSL -o /tmp/cargo-leptos.tar.gz \
      "https://github.com/leptos-rs/cargo-leptos/releases/download/v${CARGO_LEPTOS_VERSION}/cargo-leptos-${arch}-unknown-linux-gnu.tar.gz"; \
    echo "${sum}  /tmp/cargo-leptos.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/cargo-leptos.tar.gz -C /tmp; \
    install "/tmp/cargo-leptos-${arch}-unknown-linux-gnu/cargo-leptos" /usr/local/cargo/bin/; \
    rm -rf /tmp/cargo-leptos*
WORKDIR /src
COPY . .
# Builds the server binary and the web UI (WASM, JS and CSS in target/site).
# Cache mounts keep the registry and target dir between builds, so the
# outputs are copied to /out within the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo leptos build --release \
    && mkdir -p /out \
    && cp target/release/reelarr /out/reelarr \
    && cp -r target/site /out/site

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
COPY --from=builder /out/reelarr /usr/local/bin/reelarr
COPY --from=builder /out/site /usr/share/reelarr/site
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
