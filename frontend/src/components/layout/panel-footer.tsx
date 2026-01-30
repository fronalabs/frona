"use client";

import { Cog6ToothIcon, ArrowRightStartOnRectangleIcon } from "@heroicons/react/24/outline";
import { useAuth } from "@/lib/auth";
import { useRouter } from "next/navigation";

interface PanelFooterProps {
  onOpenSettings: () => void;
}

export function PanelFooter({ onOpenSettings }: PanelFooterProps) {
  const { user, logout } = useAuth();
  const router = useRouter();

  const handleLogout = () => {
    logout();
    router.replace("/login");
  };

  return (
    <div className="px-3 py-3 space-y-2">
      {user && (
        <div className="px-1 text-xs text-text-tertiary truncate">{user.email}</div>
      )}
      <div className="flex items-center gap-1">
        <button
          onClick={onOpenSettings}
          className="flex flex-1 items-center gap-2 rounded-lg px-2 py-1.5 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
        >
          <Cog6ToothIcon className="h-4 w-4" />
          Settings
        </button>
        <button
          onClick={handleLogout}
          className="flex items-center gap-2 rounded-lg px-2 py-1.5 text-sm text-text-secondary hover:bg-surface-tertiary hover:text-text-primary transition"
        >
          <ArrowRightStartOnRectangleIcon className="h-4 w-4" />
          Logout
        </button>
      </div>
    </div>
  );
}
