# Stage 1: Build frontend
FROM node:22-slim AS frontend-builder
WORKDIR /app/frontend
COPY frontend/package.json frontend/package-lock.json* ./
RUN npm ci
COPY frontend/ .
RUN npm run build

# Stage 2: Build backend (cargo-chef for dependency caching)
FROM rust:1.89-bookworm AS planner
RUN cargo install cargo-chef
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY engine/ engine/
RUN cargo chef prepare --recipe-path recipe.json

FROM rust:1.89-bookworm AS backend-builder
RUN apt-get update && apt-get install -y --no-install-recommends libclang-dev \
  && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef
WORKDIR /app
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY engine/ engine/
RUN cargo build --release -p frona

# Stage 3: Build Python packages
FROM python:3.12-slim-bookworm AS python-builder
RUN apt-get update && apt-get install -y --no-install-recommends \
  gcc g++ gfortran libopenblas-dev \
  && rm -rf /var/lib/apt/lists/*
RUN pip install --no-cache-dir --prefix=/install \
  pandas numpy scipy matplotlib seaborn scikit-learn requests beautifulsoup4 reportlab

# Stage 4: Runtime
FROM python:3.12-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends \
  ca-certificates libgfortran5 libopenblas0 \
  && rm -rf /var/lib/apt/lists/*

RUN groupadd -g 1000 frona && useradd -u 1000 -g frona -d /app -s /bin/bash frona
RUN mkdir -p /app /data && chown -R frona:frona /app /data

WORKDIR /app

COPY --from=python-builder /install /usr/local
COPY --chown=frona:frona --from=backend-builder /app/target/release/frona /app/frona
COPY --chown=frona:frona --from=frontend-builder /app/frontend/out /app/static
COPY --chown=frona:frona resources /app/share

ENV FRONA_SERVER_STATIC_DIR=/app/static
ENV FRONA_DATABASE_PATH=/data/db
ENV FRONA_SERVER_PORT=3001
ENV FRONA_STORAGE_SHARED_CONFIG_DIR=/app/share
ENV FRONA_LOG_LEVEL=info

VOLUME /data
EXPOSE 3001

USER frona
CMD ["/app/frona"]
