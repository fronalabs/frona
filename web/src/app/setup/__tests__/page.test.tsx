import React from "react";
import { beforeEach, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import SetupPage from "../page";
import { api } from "@/lib/api-client";
import type { MemoryConfig } from "@/lib/config-types";

vi.mock("next/navigation", () => ({ useRouter: () => ({ push: vi.fn() }) }));
vi.mock("@/components/require-auth", () => ({ RequireAuth: ({ children }: { children: React.ReactNode }) => children }));
vi.mock("@/lib/api-client", () => ({ api: { get: vi.fn(), put: vi.fn() } }));
vi.mock("@/components/settings/sections/providers-section", () => ({ ProvidersSection: () => null }));
vi.mock("@/components/settings/sections/models-section", () => ({ ModelsSection: () => null }));
vi.mock("@/components/settings/sections/memory-section", () => ({
  MemorySection: ({ memory, onChange }: { memory: MemoryConfig; onChange: (value: MemoryConfig) => void }) =>
    <button onClick={() => onChange({ ...memory, backend: "basic" })}>Use basic memory</button>,
}));
vi.mock("@/components/settings/sections/auth-section", () => ({ AuthSection: () => null }));
vi.mock("@/components/settings/sections/sso-section", () => ({ SsoSection: () => null }));
vi.mock("@/components/settings/sections/sandbox-section", () => ({ SandboxSettingsSection: () => null }));
vi.mock("@/components/settings/sections/browser-section", () => ({ BrowserSection: () => null }));
vi.mock("@/components/settings/sections/search-section", () => ({ SearchSection: () => null }));
vi.mock("@/components/settings/sections/voice-section", () => ({ VoiceSection: () => null }));

const config = {
  auth: { encryption_secret: { is_set: false }, access_token_expiry_secs: 900, allow_registration: true },
  memory: { backend: null, model_group: "memory", basic_compaction_secs: 7200, pkm_search_top_k: 8 },
  server: { port: 3001, static_dir: "/app/static", timezone: "", backend_url: "http://localhost:3001",
    frontend_url: "http://localhost:3000", cors_origins: "http://localhost:3000", max_concurrent_tasks: 10, max_body_size_bytes: 104857600 },
  providers: {}, models: {},
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(api.get).mockImplementation(async path => path === "/api/config"
    ? { config, authoring_document: {}, persisted_revision: "revision" } : []);
  vi.mocked(api.put).mockResolvedValue({ config, persisted_revision: "saved" });
});

async function finishSetup() {
  while (screen.queryByRole("button", { name: "Next" })) {
    fireEvent.click(screen.getByRole("button", { name: "Next" }));
  }
  fireEvent.click(screen.getByRole("button", { name: "Complete Setup" }));
  await waitFor(() => expect(api.put).toHaveBeenCalled());
}

it("saves only the generated secret and initial memory selection when other settings are untouched", async () => {
  render(<SetupPage />);
  await screen.findByRole("button", { name: "Next" });
  await finishSetup();
  expect(api.put).toHaveBeenCalledWith("/api/config", {
    patch: { auth: { encryption_secret: expect.any(String) }, memory: { backend: "pkm" } },
    expected_persisted_revision: "revision",
  });
});

it("does not copy defaults or environment values when changing timezone and memory backend", async () => {
  render(<SetupPage />);
  fireEvent.click(await screen.findByRole("button", { name: /Use browser timezone:/ }));
  for (let step = 0; step < 4; step++) fireEvent.click(screen.getByRole("button", { name: "Next" }));
  fireEvent.click(screen.getByRole("button", { name: "Use basic memory" }));
  await finishSetup();
  expect(api.put).toHaveBeenCalledWith("/api/config", {
    patch: { auth: { encryption_secret: expect.any(String) }, memory: { backend: "basic" },
      server: { timezone: Intl.DateTimeFormat().resolvedOptions().timeZone } },
    expected_persisted_revision: "revision",
  });
});
