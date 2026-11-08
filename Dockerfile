# syntax=docker/dockerfile:1

FROM rust:1-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --release --bin reelarr

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/reelarr /usr/local/bin/reelarr
ENV REELARR_PORT=7979
VOLUME ["/config", "/media", "/downloads"]
EXPOSE 7979
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["reelarr"]
