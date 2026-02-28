# Frona — Docker Compose Deployment

A ready-to-use Docker Compose setup for running Frona with browser automation and web search.

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) (with Compose v2)

## Quick Start

```bash
# 1. Copy the example environment file
cp env.example .env

# 2. Edit .env — set your encryption secret and at least one LLM API key
#    FRONA_AUTH_ENCRYPTION_SECRET=<random-secret>
#    ANTHROPIC_API_KEY=sk-ant-...

# 3. Start all services
docker compose up -d

# 4. Open Frona
open http://localhost:3001
```

## Services

| Service | Description | Port |
|---|---|---|
| **frona** | Frona server | `3001` (host) |
| **browserless** | Headless Chromium for browser automation | internal only |
| **searxng** | Meta search engine for web search | internal only |

## Configuration

- **`.env`** — API keys and secrets (required)
- **`config.yaml`** — Frona settings: model groups, providers, server options (optional — defaults work out of the box)
- **`searxng/settings.yml`** — SearXNG search engine configuration

## Data

All persistent data is stored in `./data/`:

- `data/db/` — Database
- `data/workspaces/` — Agent workspaces
- `data/files/` — Uploaded files
- `data/browser_profiles/` — Browser automation profiles

## Updating

```bash
docker compose pull
docker compose up -d
```
