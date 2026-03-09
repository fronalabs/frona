---
name: manage_service
parameters:
  action:
    type: string
    description: "Action to perform: deploy, stop, start, restart, destroy, status"
    enum:
      - deploy
      - stop
      - start
      - restart
      - destroy
      - status
  manifest:
    type: object
    description: "Full service manifest. Required for deploy/start. For stop/restart/destroy, only `id` is needed."
required:
  - action
---
Manage web services deployed from your workspace. You can build a web app (any language/runtime), then deploy it as a live service accessible via URL.

## Actions

- **deploy** — Start a new service or update an existing one. Requires user approval on first deploy or when the manifest changes.
- **stop** — Stop a running service. Process is terminated but the app entity is preserved.
- **start** — Start a previously stopped service.
- **restart** — Stop and restart a running service.
- **destroy** — Stop the service and delete the app entity permanently.
- **status** — Check the status of your services. If manifest.id is provided, returns that specific app. Otherwise returns all apps.

## Manifest Format

Always pass the full manifest with every call. The system automatically detects changes and triggers re-approval when needed.

```json
{
  "id": "my-dashboard",
  "name": "My Dashboard",
  "description": "A dashboard for monitoring metrics",
  "kind": "service",
  "command": "python app.py",
  "network_destinations": [
    {"host": "api.example.com", "port": 443}
  ],
  "health_check": {
    "path": "/healthz",
    "interval_secs": 10,
    "initial_delay_secs": 5
  },
  "restart_policy": "on_failure",
  "hibernate": true
}
```

### Key Fields

- **id** (required): Stable identifier for the service, e.g. "gold-dashboard"
- **name** (required): Human-readable name
- **kind**: "service" (default) runs a command, "static" serves files
- **command**: Startup command for service mode. Your app MUST listen on the PORT environment variable.
- **static_dir**: Directory to serve for static mode, relative to workspace (e.g. "dist/")
- **network_destinations**: Allowed outbound network destinations (host + port pairs)
- **credentials**: Vault credentials to inject as environment variables
- **health_check**: Health check configuration (path, interval, timeout)
- **restart_policy**: "on_failure" (default), "always", or "never"
- **hibernate**: Auto-hibernate after idle period (default: true). Set false for always-on services.
- **expose**: Whether to expose via reverse proxy (default: true). Set false for background workers.

### Environment Variables

For service mode, your app receives these environment variables:

- **PORT** — The port your app must listen on.
- **BASE_PATH** — The URL prefix where your app is served (e.g. `/apps/my-dashboard/`). All internal URLs, API routes, and asset references must be relative to this path.

```python
import os
port = int(os.environ.get("PORT", 8000))
base_path = os.environ.get("BASE_PATH", "/")
```

**Important:** Your app is served behind a reverse proxy at `BASE_PATH`. All fetch calls, links, and asset references in your HTML/JS must use paths relative to `BASE_PATH`, not absolute paths from `/`. For example, use `fetch('api/prices')` (relative) or `fetch(BASE_PATH + 'api/prices')`, never `fetch('/api/prices')`.

### Static Mode

For static sites, build your HTML/CSS/JS and specify the output directory:

```json
{
  "id": "docs-site",
  "name": "Documentation",
  "kind": "static",
  "static_dir": "build/"
}
```
