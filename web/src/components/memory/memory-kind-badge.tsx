const kindStyles: Record<string, string> = {
  identity: "bg-blue-500/15 text-blue-400",
  preference: "bg-purple-500/15 text-purple-400",
  fact: "bg-cyan-500/15 text-cyan-400",
  reference: "bg-amber-500/15 text-amber-400",
  episodic: "bg-rose-500/15 text-rose-400",
  procedural: "bg-green-500/15 text-green-500",
};

export function MemoryKindBadge({ kind }: { kind: string }) {
  const normalizedKind = kind.toLowerCase();
  const color = kindStyles[normalizedKind] ?? "bg-surface-tertiary text-text-secondary";
  const label = normalizedKind.charAt(0).toUpperCase() + normalizedKind.slice(1);

  return (
    <span className={`rounded-full px-2 py-0.5 text-[11px] font-medium ${color}`}>
      {label}
    </span>
  );
}
