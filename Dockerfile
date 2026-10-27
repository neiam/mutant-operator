FROM rust:1.92 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM ubuntu:rolling
COPY --from=builder /app/target/release/mutant-deployment /usr/local/bin/mutant-deployment
CMD ["mutant-deployment"]
