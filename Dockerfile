FROM rust:1-slim-bookworm AS builder

WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/grok-search-rs /usr/local/bin/grok-search-rs

ENV GROK_SEARCH_MCP_TRANSPORT=http
ENV HOST=0.0.0.0
ENV PORT=7860

EXPOSE 7860

CMD ["grok-search-rs", "serve-http"]
