FROM rust:1.83 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bullseye-slim
COPY --from=builder /app/target/release/mutant-deployment /usr/local/bin/mutant-deployment
CMD ["mutant-deployment"]
