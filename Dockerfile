# Stage 1: Build frontend
FROM node:22-slim AS frontend-builder
WORKDIR /app/frontend
COPY frontend/package.json frontend/package-lock.json* ./
RUN npm ci
COPY frontend/ .
RUN npm run build

# Stage 2: Build backend
FROM rust:latest AS backend-builder
WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY crates/ crates/
RUN cargo build --release -p frona-api

# Stage 3: Runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=backend-builder /app/target/release/frona-api /app/frona-api
COPY --from=frontend-builder /app/frontend/out /app/static

ENV STATIC_DIR=/app/static
ENV SURREAL_PATH=/data/db
ENV PORT=3001

VOLUME /data
EXPOSE 3001

CMD ["/app/frona-api"]
