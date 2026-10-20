"use client";

import { useEffect, useRef } from "react";
import type { ServerConfig } from "@/lib/config-types";
import { TextInput, NumberInput, SectionHeader, SectionPanel } from "@/components/settings/field";
import { ServerIcon } from "@heroicons/react/24/outline";

interface ServerSectionProps {
  server: ServerConfig;
  onChange: (server: ServerConfig) => void;
}

export function ServerSection({ server, onChange }: ServerSectionProps) {
  const seeded = useRef(false);

  useEffect(() => {
    if (!seeded.current && !server.base_url && !server.backend_url) {
      seeded.current = true;
      onChange({ ...server, base_url: window.location.origin });
    }
  }, [server, onChange]);

  return (
    <div>
      <SectionHeader title="Server" description="Core server settings and network configuration" icon={ServerIcon} />
      <SectionPanel>

      <NumberInput
        label="Port"
        description="Port the backend server listens on"
        value={server.port}
        onChange={(port) => onChange({ ...server, port })}
        min={1}
        max={65535}
        placeholder="3001"
      />

      <TextInput
        label="Base URL"
        description="Public URL used for generating links and callbacks"
        value={server.base_url}
        onChange={(base_url) => onChange({ ...server, base_url })}
        placeholder="https://example.com"
      />

      <TextInput
        label="Backend URL"
        description="Optional override for the backend API URL"
        value={server.backend_url}
        onChange={(backend_url) => onChange({ ...server, backend_url })}
        placeholder="https://api.example.com"
      />

      <TextInput
        label="Frontend URL"
        description="Optional override for the frontend URL"
        value={server.frontend_url}
        onChange={(frontend_url) => onChange({ ...server, frontend_url })}
        placeholder="https://app.example.com"
      />

      <TextInput
        label="External URL"
        description="Externally-reachable URL (e.g. ngrok tunnel, public domain) used as the default callback target for inbound webhooks and external service callbacks. Use this when base_url points to an internal address that callers outside your network can't reach."
        value={server.external_url}
        onChange={(external_url) => onChange({ ...server, external_url })}
        placeholder="https://abcd1234.ngrok.app"
      />

      <TextInput
        label="CORS Origins"
        description="Comma-separated list of allowed origins"
        value={server.cors_origins}
        onChange={(cors_origins) => onChange({ ...server, cors_origins })}
        placeholder="https://example.com, https://app.example.com"
      />

      <NumberInput
        label="Max Concurrent Tasks"
        description="Maximum number of tasks that can run simultaneously"
        value={server.max_concurrent_tasks}
        onChange={(max_concurrent_tasks) => onChange({ ...server, max_concurrent_tasks })}
        min={1}
        placeholder="10"
      />

      <NumberInput
        label="Max Body Size (MB)"
        description="Maximum request body size; file uploads use the separate limit below"
        value={Math.round(server.max_body_size_bytes / 1048576)}
        onChange={(mb) => onChange({ ...server, max_body_size_bytes: mb * 1048576 })}
        min={1}
        placeholder="100"
      />

      <NumberInput
        label="Max Upload File Size (MiB)"
        description="Maximum size of an uploaded file. Takes effect after restart."
        value={server.max_upload_file_size_bytes / 1048576}
        onChange={(mib) => onChange({ ...server, max_upload_file_size_bytes: mib * 1048576 })}
        min={1}
        placeholder="100"
      />

      </SectionPanel>
    </div>
  );
}
