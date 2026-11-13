"use client";

import type { InferenceConfig, SchedulerConfig, AppConfig } from "@/lib/config-types";
import { NumberInput, SectionHeader, SectionPanel } from "@/components/settings/field";
import { AdjustmentsHorizontalIcon, CpuChipIcon, ClockIcon, Square3Stack3DIcon } from "@heroicons/react/24/outline";

interface AdvancedSectionProps {
  inference: InferenceConfig;
  scheduler: SchedulerConfig;
  app: AppConfig;
  onChange: (update: { inference?: InferenceConfig; scheduler?: SchedulerConfig; app?: AppConfig }) => void;
}

export function AdvancedSection({ inference, scheduler, app, onChange }: AdvancedSectionProps) {
  return (
    <div className="space-y-6">
      <SectionHeader title="Advanced" description="Inference, scheduling, and app hosting settings" icon={AdjustmentsHorizontalIcon} />

      <SectionPanel title="Inference" icon={CpuChipIcon}>

        <NumberInput
          label="Max Tool Turns"
          description="Maximum number of tool call turns per inference request"
          value={inference.max_tool_turns}
          onChange={(max_tool_turns) => onChange({ inference: { ...inference, max_tool_turns } })}
          min={1}
          placeholder="200"
        />

        <NumberInput
          label="Max Concurrent Tool Calls"
          description="Maximum tool calls running at once per response; set to 1 for sequential execution"
          value={inference.max_concurrent_tool_calls}
          onChange={(max_concurrent_tool_calls) => onChange({ inference: { ...inference, max_concurrent_tool_calls } })}
          min={1}
        />

        <NumberInput
          label="Default Max Tokens"
          description="Default maximum tokens for LLM responses"
          value={inference.default_max_tokens}
          onChange={(default_max_tokens) => onChange({ inference: { ...inference, default_max_tokens } })}
          min={1}
          placeholder="8192"
        />

        <NumberInput
          label="Compaction Trigger (%)"
          description="Context window usage percentage that triggers compaction"
          value={inference.compaction_trigger_pct}
          onChange={(compaction_trigger_pct) => onChange({ inference: { ...inference, compaction_trigger_pct } })}
          min={1}
          max={100}
          placeholder="80"
        />

        <NumberInput
          label="History Truncation (%)"
          description="Context window usage percentage that triggers history truncation"
          value={inference.history_truncation_pct}
          onChange={(history_truncation_pct) => onChange({ inference: { ...inference, history_truncation_pct } })}
          min={1}
          max={100}
          placeholder="90"
        />
      </SectionPanel>

      <SectionPanel title="Scheduler" icon={ClockIcon}>

        <NumberInput
          label="Poll Interval (seconds)"
          description="How often the scheduler checks for pending tasks"
          value={scheduler.poll_secs}
          onChange={(poll_secs) => onChange({ scheduler: { ...scheduler, poll_secs } })}
          min={1}
          placeholder="60"
        />
      </SectionPanel>

      <SectionPanel title="Apps" icon={Square3Stack3DIcon}>

        <NumberInput
          label="Port Range Start"
          description="First port in the range allocated to hosted apps"
          value={app.port_range_start}
          onChange={(port_range_start) => onChange({ app: { ...app, port_range_start } })}
          min={1024}
          max={65535}
          placeholder="4000"
        />

        <NumberInput
          label="Port Range End"
          description="Last port in the range allocated to hosted apps"
          value={app.port_range_end}
          onChange={(port_range_end) => onChange({ app: { ...app, port_range_end } })}
          min={1024}
          max={65535}
          placeholder="4100"
        />

        <NumberInput
          label="Health Check Timeout (seconds)"
          description="How long to wait for an app to respond to health checks"
          value={app.health_check_timeout_secs}
          onChange={(health_check_timeout_secs) => onChange({ app: { ...app, health_check_timeout_secs } })}
          min={1}
          placeholder="30"
        />

        <NumberInput
          label="Max Restart Attempts"
          description="Maximum number of automatic restart attempts for a crashed app"
          value={app.max_restart_attempts}
          onChange={(max_restart_attempts) => onChange({ app: { ...app, max_restart_attempts } })}
          min={0}
          placeholder="3"
        />

        <NumberInput
          label="Hibernate After (days)"
          description="Idle time before an app is automatically hibernated"
          value={Math.round(app.hibernate_after_secs / 86400)}
          onChange={(days) => onChange({ app: { ...app, hibernate_after_secs: days * 86400 } })}
          min={1}
          placeholder="3"
        />
      </SectionPanel>
    </div>
  );
}
