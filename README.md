<p align="center">
    <a href="https://docs.frona.ai/" target="_blank">
        <img width="300" src="https://docs.frona.ai/logo-light.svg" alt="Frona AI">
    </a>
</p>

Frona is a self-hosted AI agent platform. You create autonomous agents, give them tools, and talk to them through a chat interface. Agents act on their own. They browse the web, run code, develop applications, search the internet, make phone calls, delegate work to each other, and remember context across conversations. You give them a task and they figure out how to get it done.

You deploy Frona on your own infrastructure and keep full control of your data. The platform is built from the ground up with security in mind, and the engine is written in Rust. So it's fast, lightweight, and runs everything in a single process.

## Security First

AI agents are powerful. They can execute code, browse websites, and access your data. No platform can make LLMs perfectly safe. They will make mistakes. The goal is to isolate those mistakes and reduce the blast radius when they happen.

- **Sandboxed execution:** when agents run shell commands, they execute inside a sandbox that restricts filesystem access to the agent's workspace directory, controls network access per agent, and enforces execution timeouts
- **Agent isolation:** each agent gets its own set of tools, its own workspace directory, and its own credentials. Create narrow, purpose-built agents instead of one agent that can do everything
- **Isolated browser sessions:** each user gets separate browser profiles. Different credentials get separate browser states. One user's cookies and sessions are never visible to another
- **Kill switch:** gives you control to stop all running agent operations instantly
- **Self-hosted by design:** your data lives on your servers. You choose which LLM provider to use, and traffic goes directly from your instance to that provider

## Features

- **Autonomous agents with tools:** agents decide which tools to use and execute multi-step tasks on their own. Agents can also build their own tools.
- **Browser automation:** headless Chrome via Browserless for navigating websites, filling forms, and extracting data
- **Web search:** built-in search via SearXNG, Tavily, or Brave Search
- **Code execution:** sandboxed shell commands with filesystem and network restrictions per agent
- **Voice calls:** outbound phone calls via Twilio (optional)
- **Persistent memory:** agents remember facts across conversations with automatic compaction
- **Agent-to-agent delegation:** agents hand off tasks to specialized agents
- **Real-time streaming:** token-by-token response streaming over Server-Sent Events
- **Single-container deployment:** the entire backend (API server, embedded database, scheduler, tool execution) runs in one rootless Docker container

## Core Concepts

- **Agents** are the main building blocks. Each agent has a name, a system prompt that defines its behavior, a model group that determines which LLM it uses, and a list of tools it can access.
- **Memory** lets agents remember things across conversations. There are user-scoped facts (shared across all agents) and agent-scoped facts (private to one agent). The platform automatically compacts and deduplicates memories over time.
- **Tools** are capabilities you give to agents. Browser automation, web search, file operations, shell commands, voice calls, task scheduling, and more. Tools run server-side and return results to the agent.
- **Tasks** represent units of work. They can be direct (run immediately), delegated (from one agent to another), or scheduled (recurring via cron expressions).
- **Chat** is how you interact with agents. Each conversation belongs to one agent, but multiple agents can contribute to it through delegation. Messages stream in real-time over Server-Sent Events.
- **Spaces** are groups of chats that share the same context. When you link conversations to a space, the platform summarizes those conversations and feeds the context back into new chats.

## Quickstart

See the [docker-compose example](examples/docker-compose) to get started, take a look at [screenshots](https://docs.frona.ai/platform/screenshots/) to see the platform in action, or check out the [documentation](https://docs.frona.ai) for more details.

## Model Providers

Frona connects to any of the following LLM providers. Set the corresponding API key in your env and the provider is auto-discovered.

| Provider | Environment Variable |
|---|---|
| Anthropic | `ANTHROPIC_API_KEY` |
| OpenAI | `OPENAI_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| DeepSeek | `DEEPSEEK_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| Cohere | `COHERE_API_KEY` |
| xAI (Grok) | `XAI_API_KEY` |
| Groq | `GROQ_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |
| Together | `TOGETHER_API_KEY` |
| Perplexity | `PERPLEXITY_API_KEY` |
| Hyperbolic | `HYPERBOLIC_API_KEY` |
| Moonshot | `MOONSHOT_API_KEY` |
| Hugging Face | `HUGGINGFACE_API_KEY` |
| Mira | `MIRA_API_KEY` |
| Galadriel | `GALADRIEL_API_KEY` |
| Ollama (local) | `OLLAMA_API_BASE_URL` |

## Search Providers

Agents can search the web using any of the following providers. Set `FRONA_SEARCH_PROVIDER` or let Frona auto-detect from available API keys.

| Provider | Environment Variable |
|---|---|
| SearXNG (self-hosted) | `FRONA_SEARCH_SEARXNG_BASE_URL` |
| Tavily | `TAVILY_API_KEY` |
| Brave Search | `BRAVE_API_KEY` |

## Voice Providers

Agents can make and receive phone calls. Set `FRONA_VOICE_PROVIDER` or let Frona auto-detect from available credentials.

| Provider | Environment Variables |
|---|---|
| Twilio | `FRONA_VOICE_TWILIO_ACCOUNT_SID`, `FRONA_VOICE_TWILIO_AUTH_TOKEN`, `FRONA_VOICE_TWILIO_FROM_NUMBER` |

## Architecture

Frona has two main components:

- **Engine:** a Rust backend (Axum) that handles agents, chat, tools, authentication, and an embedded SurrealDB database with RocksDB storage
- **Frontend:** a Next.js application that provides the chat interface, agent management, and workspace UI

External services plug in for specific capabilities:

- **Browserless:** headless Chrome for browser automation
- **SearXNG:** web search
- **Twilio:** voice calls (optional)

Everything runs in Docker containers. A typical deployment is a single `docker-compose.yml` that brings up the engine, frontend, and supporting services.

## Development

All commands use [mise](https://mise.jdx.dev/) as the task runner:

```bash
mise run dev              # Run backend + frontend in parallel
mise run dev:backend      # Rust server with cargo-watch hot-reload (port 3001)
mise run dev:frontend     # Next.js dev server (port 3000)
mise run build            # Full production build
mise run check            # cargo check --workspace
mise run lint             # clippy + next lint
mise run test             # cargo test --workspace
```

Start external services (Browserless, SearXNG) for local development:

```bash
docker compose up -d
```

## License

Frona is licensed under the [Business Source License 1.1](LICENSE). You can use, modify, and self-host it freely. The only restriction is that you may not use it to provide an AI agent platform as a service to third parties. On 2029-02-28, the license converts to Apache 2.0.
