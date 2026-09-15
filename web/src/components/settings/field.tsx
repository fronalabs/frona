"use client";

import { useId, useState } from "react";
import { InformationCircleIcon } from "@heroicons/react/16/solid";
import * as Tooltip from "@radix-ui/react-tooltip";
import type { SensitiveField } from "@/lib/config-types";
import { isSensitiveSet } from "@/lib/config-types";

export function HelpTip({ content }: { content: string }) {
  return (
    <Tooltip.Provider delayDuration={200}>
      <Tooltip.Root>
        <Tooltip.Trigger asChild>
          <button type="button" className="inline-flex items-center p-0 leading-none">
            <InformationCircleIcon className="h-3.5 w-3.5 text-text-tertiary hover:text-text-secondary transition-colors" />
          </button>
        </Tooltip.Trigger>
        <Tooltip.Portal>
          <Tooltip.Content
            side="top"
            sideOffset={4}
            className="z-50 max-w-xs rounded-lg bg-surface-secondary border border-border px-3 py-2 text-xs text-text-secondary shadow-lg animate-in fade-in-0 zoom-in-95"
          >
            {content}
            <Tooltip.Arrow className="fill-surface-secondary" />
          </Tooltip.Content>
        </Tooltip.Portal>
      </Tooltip.Root>
    </Tooltip.Provider>
  );
}

interface FieldProps {
  label: string;
  description?: string;
  htmlFor?: string;
  children: React.ReactNode;
}

export function Field({ label, description, htmlFor, children }: FieldProps) {
  return (
    <div className="space-y-1">
      <label htmlFor={htmlFor} className="inline-flex items-center gap-1 text-sm font-medium text-text-secondary">
        {label}
        {description && (
          <HelpTip content={description} />
        )}
      </label>
      {children}
    </div>
  );
}

interface TextInputProps {
  label: string;
  description?: string;
  value: string | null | undefined;
  onChange: (value: string) => void;
  placeholder?: string;
  type?: string;
  disabled?: boolean;
  required?: boolean;
  autoComplete?: string;
  onBlur?: () => void;
}

export function TextInput({ label, description, value, onChange, placeholder, type = "text", disabled, required, autoComplete, onBlur }: TextInputProps) {
  const id = useId();
  return (
    <Field label={label} description={description} htmlFor={id}>
      <input
        id={id}
        type={type}
        disabled={disabled}
        required={required}
        autoComplete={autoComplete}
        onBlur={onBlur}
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none disabled:cursor-not-allowed disabled:opacity-50"
      />
    </Field>
  );
}

interface NumberInputProps {
  label: string;
  description?: string;
  value: number | null | undefined;
  onChange: (value: number) => void;
  min?: number;
  max?: number;
  step?: number;
  placeholder?: string;
  className?: string;
}

export function NumberInput({ label, description, value, onChange, min, max, step, placeholder, className }: NumberInputProps) {
  return (
    <Field label={label} description={description}>
      <input
        type="number"
        value={typeof value === "number" && Number.isFinite(value) ? value : ""}
        onChange={(e) => {
          const raw = e.target.value;
          if (raw === "") return;
          const n = Number(raw);
          if (Number.isFinite(n)) onChange(n);
        }}
        min={min}
        max={max}
        step={step}
        placeholder={placeholder}
        className={`rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none ${className ?? "w-full"}`}
      />
    </Field>
  );
}

interface ToggleProps {
  label: string;
  description?: string;
  value: boolean;
  onChange: (value: boolean) => void;
  warning?: string;
  disabled?: boolean;
}

export function Toggle({ label, description, value, onChange, warning, disabled }: ToggleProps) {
  const id = useId();
  return (
    <Field label={label} description={description} htmlFor={id}>
      <div className="flex items-center gap-3">
        <button
          type="button"
          id={id}
          role="switch"
          aria-checked={value}
          disabled={disabled}
          onClick={() => onChange(!value)}
          className={`relative inline-flex h-6 w-11 shrink-0 cursor-pointer rounded-full border-2 border-transparent transition-colors disabled:cursor-not-allowed disabled:opacity-50 ${
            value ? "bg-accent" : "bg-surface-tertiary"
          }`}
        >
          <span
            className={`pointer-events-none inline-block h-5 w-5 rounded-full bg-surface shadow transform transition-transform ${
              value ? "translate-x-5" : "translate-x-0"
            }`}
          />
        </button>
        {warning && value && (
          <span className="text-xs text-warning">{warning}</span>
        )}
      </div>
    </Field>
  );
}

interface SensitiveInputProps {
  label: string;
  description?: string;
  value: SensitiveField;
  onChange: (value: string) => void;
  placeholder?: string;
  onGenerate?: () => string;
}

export function SensitiveInput({ label, description, value, onChange, placeholder, onGenerate }: SensitiveInputProps) {
  const [editing, setEditing] = useState(false);
  const [localValue, setLocalValue] = useState("");
  const [generated, setGenerated] = useState(false);
  const isSet = isSensitiveSet(value);
  const isRedacted = typeof value === "object" && value !== null && "is_set" in value;

  if (isRedacted && isSet && !editing) {
    return (
      <Field label={label} description={description}>
        <div className="flex items-center gap-2">
          <span className={`text-sm ${isSet ? "text-text-primary" : "text-text-tertiary"}`}>
            {isSet ? "Configured" : "Not set"}
          </span>
          <button
            type="button"
            onClick={() => setEditing(true)}
            className="text-xs text-accent hover:underline"
          >
            Change
          </button>
        </div>
      </Field>
    );
  }

  const displayValue = editing ? localValue : (typeof value === "string" ? value : "");

  return (
    <Field label={label} description={description}>
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <input
            type={generated ? "text" : "password"}
            value={displayValue}
            onChange={(e) => {
              setGenerated(false);
              if (editing) setLocalValue(e.target.value);
              onChange(e.target.value);
            }}
            placeholder={placeholder}
            className={`w-full rounded-lg border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none ${
              generated ? "border-accent" : "border-border"
            }`}
          />
          {generated && (
            <span className="absolute right-2 top-1/2 -translate-y-1/2 text-xs text-accent font-medium">
              Generated
            </span>
          )}
        </div>
        {onGenerate && (
          <button
            type="button"
            onClick={() => {
              const val = onGenerate();
              if (editing) setLocalValue(val);
              onChange(val);
              setGenerated(true);
              setTimeout(() => setGenerated(false), 3000);
            }}
            className="shrink-0 rounded-lg bg-accent px-3 py-2 text-xs font-medium text-surface hover:opacity-90 transition"
          >
            Generate
          </button>
        )}
      </div>
    </Field>
  );
}

interface SelectInputProps {
  label: string;
  description?: string;
  value: string | null | undefined;
  onChange: (value: string | null) => void;
  options: { value: string; label: string }[];
  allowEmpty?: boolean;
}

export function SelectInput({ label, description, value, onChange, options, allowEmpty = true }: SelectInputProps) {
  return (
    <Field label={label} description={description}>
      <select
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value || null)}
        className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary focus:border-accent focus:outline-none"
      >
        {allowEmpty && <option value="">None</option>}
        {options.map((opt) => (
          <option key={opt.value} value={opt.value}>{opt.label}</option>
        ))}
      </select>
    </Field>
  );
}

export function SectionHeader({ title, description, icon: Icon }: { title: string; description?: string; icon?: React.ComponentType<React.SVGProps<SVGSVGElement>> }) {
  return (
    <div className="mb-5 pb-3 border-b border-border flex items-end justify-between gap-3">
      <div>
        <h3 className="text-lg font-semibold text-text-primary">{title}</h3>
        {description && <p className="text-sm text-text-tertiary mt-1">{description}</p>}
      </div>
      {Icon && <Icon className="h-10 w-10 text-text-tertiary shrink-0" />}
    </div>
  );
}

export function SectionPanel({ title, icon: Icon, helpTip, children, className }: { title?: string; icon?: React.ComponentType<React.SVGProps<SVGSVGElement>>; helpTip?: string; children: React.ReactNode; className?: string }) {
  return (
    <div className={`rounded-xl border border-border bg-surface-secondary p-5 space-y-4 ${className ?? ""}`}>
      {title && (
        <div className="-mt-1 pb-3 border-b border-border flex items-center gap-2">
          {Icon && <Icon className="h-4.5 w-4.5 text-text-tertiary" />}
          <h4 className="text-base font-semibold text-text-primary">{title}</h4>
          {helpTip && <HelpTip content={helpTip} />}
        </div>
      )}
      {children}
    </div>
  );
}
