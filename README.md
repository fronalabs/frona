<p align="center">
    <a href="https://docs.frona.ai/" target="_blank">
        <img width="300" src="https://docs.frona.ai/logo-light.svg" alt="Frona AI">
    </a>
</p>

<p align="center">
    <a href="https://github.com/fronalabs/frona/releases"><img src="https://img.shields.io/github/v/release/fronalabs/frona?style=flat-square&color=blue" alt="Latest release"></a>
    <img src="https://img.shields.io/badge/built_with-Rust-dea584?style=flat-square&logo=rust" alt="Built with Rust">
    <a href="https://github.com/fronalabs/frona/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-BSL_1.1-blue?style=flat-square" alt="License"></a>
    <a href="https://github.com/fronalabs/frona/stargazers"><img src="https://img.shields.io/github/stars/fronalabs/frona?style=flat-square&logo=github" alt="GitHub stars"></a>
    <a href="https://github.com/fronalabs/frona/pkgs/container/frona"><img src="https://img.shields.io/badge/ghcr.io-fronalabs%2Ffrona-2496ed?style=flat-square&logo=docker&logoColor=white" alt="Container image"></a>
    <a href="https://docs.frona.ai/"><img src="https://img.shields.io/badge/docs-frona.ai-8a3ffc?style=flat-square" alt="Documentation"></a>
</p>

Frona is a personal AI assistant. You create autonomous agents that browse the web, run code, build applications, make phone calls, connect to messaging channels, delegate work to each other, and remember context across conversations, all within sandboxed environments with controlled access to your files, network, and credentials. You give them a task and they figure out how to get it done.

You deploy Frona on your own infrastructure and keep full control of your data. The platform is built from the ground up with security in mind, and the engine is written in Rust. So it's fast, lightweight, and runs everything in a single process.

> Comparing Frona to other open-source agent platforms? See [Frona vs. OpenClaw vs. Hermes Agent](https://docs.frona.ai/platform/comparison.html).

## Security First

AI agents are powerful. They can execute code, browse websites, and access your data. No platform can make LLMs perfectly safe. They will make mistakes. The goal is to isolate those mistakes and reduce the blast radius when they happen.

- **Per-principal sandboxing:** every actor (agent, MCP server, app, channel) is its own principal with its own policies. Each CLI tool call, each MCP server, each deployed app runs in its own sandboxed Linux process with policy-driven syscall filtering. There's no Docker container per agent and no daemon to manage; the engine spawns and reaps sandboxes on demand
- **One policy engine:** tool access *and* sandbox rules (read/write paths, network destinations, port binds) are written in the same policy language and evaluated by a single engine. One language, one decision point, no glue code between authorization and isolation
- **Isolated browser sessions:** each user gets separate browser profiles. Different credentials get separate browser states. One user's cookies and sessions are never visible to another
- **Credential vault:** agents request credentials when they need them, and you approve or deny in real time. Supports 1Password, Bitwarden, HashiCorp Vault, and KeePass. Secrets are never stored in agent memory or sent to LLM providers
- **Dual LLM dispatch on inbound:** untrusted channel messages can be routed to a quarantined LLM with a restricted tool registry, so a hostile inbound message can't talk the agent into running tools or leaking data on its behalf
- **Self-hosted by design:** your data lives on your servers. You choose which LLM provider to use, and traffic goes directly from your instance to that provider

## Features

- **Two memory backends:** use lightweight Basic memory or ontology-backed PKM, which turns conversations into evidence-grounded memories, typed entities and relationships, reusable playbooks, and searchable Markdown pages. Inspect the resulting knowledge graph and consolidation history from the Memory UI
- **Autonomous agents with tools:** agents decide which tools to use and execute multi-step tasks on their own. Agents can also build their own tools
- **Channels:** connect agents through Telegram, Slack, Discord, WhatsApp, Signal, or Twilio SMS so the same agent, with the same memory and tools, follows you outside the web UI. Channels support device pairing, policy-gated Message and Signal modes, interactive approval prompts, and automatic reconnection
- **Signals:** an agent can pause a conversation and wait for a matching inbound (a 2FA code, a reply, a class of message) and resume automatically when something arrives, or run continuous monitors with structured results
- **MCP with bridge mode:** install [Model Context Protocol](https://modelcontextprotocol.io) servers from the public registry in a click. Bridge mode advertises a single `mcpctl` CLI to the LLM instead of every MCP tool individually, saving thousands of tokens per turn on agents with many servers connected
- **Browser automation:** headless Chrome via Browserless for navigating websites, filling forms, and extracting data. Persistent browser profiles keep sessions across conversations
- **Web search:** built-in search via SearXNG, Tavily, or Brave Search
- **Code execution:** sandboxed shell, Python, and Node.js with per-principal filesystem, network, and resource restrictions
- **App deployment:** agents build and deploy web applications and services on your behalf, with an approval workflow before anything goes live
- **Skills:** instruction packages that teach agents new capabilities. Install shared skills or create agent-specific ones
- **Scheduling and heartbeats:** recurring tasks via cron and agent-managed heartbeat checklists for ongoing monitoring
- **Voice calls:** outbound phone calls via Twilio with speech recognition and DTMF navigation (optional)
- **Agent-to-agent delegation:** agents hand off tasks to specialized agents and get results back
- **Spaces:** group conversations that share context. The platform summarizes linked conversations and feeds the context into new chats
- **Usage and cost visibility:** monitor tokens, cost, context-window usage, and model fallbacks live in each chat, with per-user dashboards for spend, latency, cache efficiency, models, and call types
- **Commands:** use slash commands and mentions to invoke built-in actions, installed skills, or other agents directly from chat
- **Notifications:** agents push status updates (task finished, app deployed, credential needs approval) into a feed in the top bar so nothing important gets lost
- **Real-time streaming:** token-by-token response streaming over Server-Sent Events
- **SSO:** OpenID Connect support for single sign-on with Google, Keycloak, and other OIDC providers
- **Single-container deployment:** the entire backend (API server, embedded database, scheduler, tool execution) runs in one rootless OCI container (compatible with Docker, Podman, and other OCI runtimes). No per-agent containers, even at scale

## Core Concepts

- **Agents** are the main building blocks. Each agent has a name, a system prompt that defines its behavior, a model group that determines which LLM it uses, and a list of tools it can access. Frona ships with built-in agents (Assistant, Researcher, Developer, Receptionist) and you can create your own.
- **Policies** authorize every action: tool calls, delegations, file reads, network connections, and inbound channel messages. The same engine controls tool access and sandbox rules, so authorization lives in one place.
- **Memory** persists knowledge across conversations. Basic memory maintains compact user-scoped facts shared across agents and private agent-scoped notes. PKM builds a user-scoped knowledge graph of grounded atomic memories, entities, relationships, attributes, playbooks, and readable Markdown pages backed by an ontology.
- **Tools** are capabilities you give to agents. Browser automation, web search, file operations, shell commands, voice calls, task scheduling, and more. Tools run server-side and return results to the agent.
- **MCP servers** are first-class citizens. Each runs in its own sandbox as its own principal with its own filesystem, network, and resource policies, and surfaces its tools to agents through bridge mode by default.
- **Channels** connect an agent to messaging providers. Each channel is bound to a single agent and space, with policy-gated `receive_message` and `receive_signal` actions deciding what an inbound is allowed to do.
- **Signals** are "wait for X to happen" tasks. An agent calls `await_signal` and the conversation resumes when an inbound message matches.
- **Tasks** represent units of work. They can be direct (run immediately), delegated (from one agent to another), or scheduled (recurring via cron expressions).
- **Chat** is how you interact with agents. Each conversation belongs to one agent, but multiple agents can contribute to it through delegation. Messages stream in real-time over Server-Sent Events.
- **Spaces** are groups of chats that share the same context. When you link conversations to a space, the platform summarizes those conversations and feeds the context back into new chats.
- **Skills** are instruction packages you install on agents. They can be built-in, shared across all agents, or scoped to a single agent.

## Quickstart

You'll need an OCI runtime with Compose v2 support, such as [Docker](https://docs.docker.com/get-docker/) or [Podman](https://podman.io/).

```yaml
# docker-compose.yml
services:
  frona:
    image: ghcr.io/fronalabs/frona:latest
    ports:
      - "3001:3001"
    volumes:
      - ./data:/app/data
    environment:
      - FRONA_BROWSER_WS_URL=ws://browserless:3333
      - FRONA_SEARCH_SEARXNG_BASE_URL=http://searxng:8080
    # Only needed if you plan to restrict agent network destinations.
    # See https://docs.frona.ai/platform/security/sandbox.html
    security_opt:
      - seccomp:unconfined
    depends_on:
      - browserless
      - searxng
    restart: unless-stopped

  browserless:
    image: ghcr.io/browserless/chromium:v2.42.0
    environment:
      - MAX_CONCURRENT_SESSIONS=10
      - PREBOOT_CHROME=true
    volumes:
      - ./data/browser_profiles:/profiles
    restart: unless-stopped

  searxng:
    image: searxng/searxng:latest
    environment:
      - SEARXNG_BASE_URL=http://searxng:8080
      - SEARXNG_SECRET=change-me-to-something-random
    configs:
      - source: searxng-settings
        target: /etc/searxng/settings.yml
    restart: unless-stopped

configs:
  searxng-settings:
    content: |
      use_default_settings: true
      search:
        formats:
          - html
          - json
```

```bash
docker compose up -d   # or: podman compose up -d
open http://localhost:3001
```

The setup wizard will guide you through creating your account and configuring your LLM provider.

See the [docker-compose example](examples/docker-compose) for a full deployment with environment configuration, the [documentation](https://docs.frona.ai) for detailed guides, or [screenshots](https://docs.frona.ai/platform/screenshots.html) to see the platform in action.

## Providers

Frona auto-discovers providers from your configuration and routes different tasks to the right one. Configure them in the [config file](https://docs.frona.ai/platform/deployment/config-file.html).

**LLM:** Anthropic, OpenAI, Google Gemini, DeepSeek, Mistral, Cohere, xAI (Grok), Groq, OpenRouter, Together, Perplexity, Hyperbolic, Moonshot, Hugging Face, Mira, Galadriel, Ollama (local).

**Search:** SearXNG (self-hosted), Tavily, Brave Search.

**Voice:** Twilio.

**Channels:** Telegram, Slack, Discord, WhatsApp Cloud API, WhatsApp Personal, Signal, and Twilio SMS. WhatsApp Personal and Signal use linked-device integrations; review the provider-specific notices in the [channel documentation](https://docs.frona.ai/platform/agents/channels/overview.html) before enabling them.

## Architecture

Frona ships as a single rootless OCI image containing two main components:

- **Engine:** a Rust backend (Axum) that handles agents, chat, tools, authentication, the policy engine, and an embedded SurrealDB database with RocksDB storage. The engine spawns sandboxed child processes for tool calls, MCP servers, and apps; it does not spin up containers per agent
- **Frontend:** a statically exported Next.js application, served by the engine, that provides the chat interface, agent management, and workspace UI

External services plug in for specific capabilities:

- **Browserless:** headless Chrome for browser automation
- **SearXNG:** web search
- **Twilio:** voice calls and SMS (optional)

The Frona application runs in one OCI container and works with any OCI-compatible runtime (Docker, Podman, etc.). A typical `docker-compose.yml` runs that container alongside optional supporting services such as Browserless and SearXNG. See the [Kubernetes example](examples/kubernetes) for cluster deployments.

## Documentation

- [Overview](https://docs.frona.ai/platform/overview.html) - what Frona is and how it works
- [Quickstart](https://docs.frona.ai/platform/quickstart.html) - get running with Docker in minutes
- [Comparison](https://docs.frona.ai/platform/comparison.html) - Frona vs. OpenClaw vs. Hermes Agent
- [Agents](https://docs.frona.ai/platform/agents/overview.html) - agent types, configuration, and delegation
- [Channels](https://docs.frona.ai/platform/agents/channels/overview.html) - Telegram, Slack, Discord, WhatsApp, Signal, SMS, pairing, and dispatch modes
- [Memory](https://docs.frona.ai/platform/agents/memory/overview.html) - Basic and PKM memory backends
- [Personal Knowledge Management](https://docs.frona.ai/platform/agents/memory/pkm.html) - grounded, ontology-backed long-term memory
- [Usage and Costs](https://docs.frona.ai/platform/agents/chat/usage.html) - token, cost, latency, and cache dashboards
- [Signals](https://docs.frona.ai/platform/agents/signals.html) - pause-and-resume on inbound messages
- [Skills](https://docs.frona.ai/platform/agents/skills/overview.html) - extend agents with reusable instruction packages
- [Apps](https://docs.frona.ai/platform/agents/apps/overview.html) - let agents build and deploy applications
- [Tools](https://docs.frona.ai/platform/tools/overview.html) - browser, search, CLI, voice, and more
- [MCP](https://docs.frona.ai/platform/tools/mcp/overview.html) - install MCP servers and bridge mode
- [Sandbox](https://docs.frona.ai/platform/security/sandbox.html) - filesystem, network, and resource controls
- [Policies](https://docs.frona.ai/platform/security/policies.html) - policy reference for tools and sandbox rules
- [Credentials](https://docs.frona.ai/platform/credentials/overview.html) - vault integration and approval workflows
- [Deployment](https://docs.frona.ai/platform/deployment/docker-compose.html) - Docker Compose and Kubernetes guides

## Development

All commands use [mise](https://mise.jdx.dev/) as the task runner:

```bash
mise run container:dev    # Run the containerized dev stack with hot-reload
mise run container:prod   # Build and run the production container stack

mise run check            # Check all Rust crates
mise run lint             # Check formatting and lint backend + frontend
mise run test             # Run the workspace test suite
```

See [mise.toml](mise.toml) for all available targets.

## License

Frona is licensed under the [Business Source License 1.1](LICENSE). You can use, modify, and self-host it freely. The only restriction is that you may not use it to provide an AI agent platform as a service to third parties. On 2029-02-28, the license converts to Apache 2.0.
