FROM --platform=linux/arm64 rust:1.89.0 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM --platform=linux/arm64 debian:13-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/broadcast-server /usr/local/bin/broadcast-server
EXPOSE 8080
CMD ["broadcast-server"]
