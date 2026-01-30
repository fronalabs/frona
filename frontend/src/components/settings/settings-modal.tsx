"use client";

import { useState, useEffect, useCallback } from "react";
import { XMarkIcon, PlusIcon, TrashIcon } from "@heroicons/react/24/outline";
import { useTheme } from "@/lib/theme";
import { useAuth } from "@/lib/auth";
import { api } from "@/lib/api-client";
import type { CredentialResponse } from "@/lib/types";

interface SettingsModalProps {
  open: boolean;
  onClose: () => void;
}

const themeModes = [
  { value: "system" as const, label: "System" },
  { value: "light" as const, label: "Light" },
  { value: "dark" as const, label: "Dark" },
];

type CredentialType = "BrowserProfile" | "UsernamePassword";

export function SettingsModal({ open, onClose }: SettingsModalProps) {
  const { mode, setMode } = useTheme();
  const { user } = useAuth();

  const [credentials, setCredentials] = useState<CredentialResponse[]>([]);
  const [showForm, setShowForm] = useState(false);
  const [credType, setCredType] = useState<CredentialType>("BrowserProfile");
  const [name, setName] = useState("");
  const [provider, setProvider] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const fetchCredentials = useCallback(async () => {
    try {
      const data = await api.get<CredentialResponse[]>("/api/credentials");
      setCredentials(data);
    } catch {
      // ignore fetch errors
    }
  }, []);

  useEffect(() => {
    if (open) {
      fetchCredentials();
    }
  }, [open, fetchCredentials]);

  const resetForm = () => {
    setShowForm(false);
    setCredType("BrowserProfile");
    setName("");
    setProvider("");
    setUsername("");
    setPassword("");
  };

  const handleCreate = async () => {
    if (!name.trim() || !provider.trim()) return;
    if (credType === "UsernamePassword" && (!username.trim() || !password.trim())) return;

    setSubmitting(true);
    try {
      const data =
        credType === "BrowserProfile"
          ? { type: "BrowserProfile" as const }
          : { type: "UsernamePassword" as const, data: { username, password } };

      await api.post("/api/credentials", { name, provider, data });
      resetForm();
      await fetchCredentials();
    } catch {
      // ignore create errors
    } finally {
      setSubmitting(false);
    }
  };

  const handleDelete = async (id: string) => {
    try {
      await api.delete(`/api/credentials/${id}`);
      await fetchCredentials();
    } catch {
      // ignore delete errors
    }
  };

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      <div className="absolute inset-0 bg-black/40" onClick={onClose} />
      <div className="relative w-full max-w-md rounded-xl bg-surface border border-border p-6 shadow-lg max-h-[85vh] overflow-y-auto">
        <div className="flex items-center justify-between mb-6">
          <h2 className="text-lg font-semibold text-text-primary">Settings</h2>
          <button
            onClick={onClose}
            className="rounded-lg p-1.5 text-text-secondary hover:bg-surface-tertiary transition"
          >
            <XMarkIcon className="h-5 w-5" />
          </button>
        </div>

        <div className="space-y-6">
          <div>
            <label className="block text-sm font-medium text-text-secondary mb-2">
              Theme
            </label>
            <div className="flex gap-2">
              {themeModes.map(({ value, label }) => (
                <button
                  key={value}
                  onClick={() => setMode(value)}
                  className={`flex-1 rounded-lg px-3 py-2 text-sm font-medium transition ${
                    mode === value
                      ? "bg-accent text-surface"
                      : "bg-surface-secondary text-text-secondary hover:bg-surface-tertiary"
                  }`}
                >
                  {label}
                </button>
              ))}
            </div>
          </div>

          {user && (
            <div>
              <label className="block text-sm font-medium text-text-secondary mb-2">
                Profile
              </label>
              <div className="rounded-lg bg-surface-secondary p-3 space-y-1">
                <p className="text-sm text-text-primary">{user.name}</p>
                <p className="text-xs text-text-tertiary">{user.email}</p>
              </div>
            </div>
          )}

          <div>
            <div className="flex items-center justify-between mb-2">
              <label className="text-sm font-medium text-text-secondary">
                Credentials
              </label>
              {!showForm && (
                <button
                  onClick={() => setShowForm(true)}
                  className="rounded-lg p-1 text-text-secondary hover:bg-surface-tertiary transition"
                >
                  <PlusIcon className="h-4 w-4" />
                </button>
              )}
            </div>

            {credentials.length > 0 && (
              <div className="rounded-lg bg-surface-secondary divide-y divide-border">
                {credentials.map((cred) => (
                  <div key={cred.id} className="flex items-center justify-between p-3">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <p className="text-sm text-text-primary truncate">{cred.name}</p>
                        <span className="shrink-0 rounded bg-surface-tertiary px-1.5 py-0.5 text-[10px] font-medium text-text-tertiary">
                          {cred.data.type === "BrowserProfile" ? "Browser" : "Password"}
                        </span>
                      </div>
                      <p className="text-xs text-text-tertiary truncate">{cred.provider}</p>
                    </div>
                    <button
                      onClick={() => handleDelete(cred.id)}
                      className="shrink-0 ml-2 rounded-lg p-1 text-text-tertiary hover:text-red-500 hover:bg-surface-tertiary transition"
                    >
                      <TrashIcon className="h-4 w-4" />
                    </button>
                  </div>
                ))}
              </div>
            )}

            {credentials.length === 0 && !showForm && (
              <p className="text-xs text-text-tertiary">No credentials yet.</p>
            )}

            {showForm && (
              <div className="mt-2 rounded-lg border border-border p-3 space-y-3">
                <div className="flex gap-2">
                  {(["BrowserProfile", "UsernamePassword"] as const).map((t) => (
                    <button
                      key={t}
                      onClick={() => setCredType(t)}
                      className={`flex-1 rounded-lg px-3 py-1.5 text-xs font-medium transition ${
                        credType === t
                          ? "bg-accent text-surface"
                          : "bg-surface-secondary text-text-secondary hover:bg-surface-tertiary"
                      }`}
                    >
                      {t === "BrowserProfile" ? "Browser Profile" : "Username & Password"}
                    </button>
                  ))}
                </div>
                <input
                  type="text"
                  placeholder="Name"
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none"
                />
                <input
                  type="text"
                  placeholder="Provider"
                  value={provider}
                  onChange={(e) => setProvider(e.target.value)}
                  className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none"
                />
                {credType === "UsernamePassword" && (
                  <>
                    <input
                      type="text"
                      placeholder="Username"
                      value={username}
                      onChange={(e) => setUsername(e.target.value)}
                      className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none"
                    />
                    <input
                      type="password"
                      placeholder="Password"
                      value={password}
                      onChange={(e) => setPassword(e.target.value)}
                      className="w-full rounded-lg border border-border bg-surface px-3 py-2 text-sm text-text-primary placeholder:text-text-tertiary focus:border-accent focus:outline-none"
                    />
                  </>
                )}
                <div className="flex gap-2 justify-end">
                  <button
                    onClick={resetForm}
                    className="rounded-lg px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-surface-tertiary transition"
                  >
                    Cancel
                  </button>
                  <button
                    onClick={handleCreate}
                    disabled={submitting}
                    className="rounded-lg bg-accent px-3 py-1.5 text-xs font-medium text-surface hover:bg-accent/90 transition disabled:opacity-50"
                  >
                    {submitting ? "Adding..." : "Add"}
                  </button>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
