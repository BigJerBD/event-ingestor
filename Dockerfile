FROM rust:1.85 as builder
WORKDIR /code/

COPY Cargo.lock .
COPY Cargo.toml .
COPY src ./src
COPY cloud-pubsub cloud-pubsub

RUN cargo install --path .


FROM debian:stable-slim

RUN apt-get update \
 && apt-get install -y openssl  ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/cargo/bin/event-ingestor /usr/local/bin/

CMD ["event-ingestor"]