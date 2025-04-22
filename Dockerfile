FROM rust:1.83 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:buster-slim
COPY --from=builder /app/target/release/mutantops /usr/local/bin/mutantops
CMD ["mutantops"]
