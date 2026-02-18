"use client";

import { useEffect } from "react";
import { useRouter } from "next/navigation";
import { useAuth } from "@/lib/auth";

export default function SsoCallbackPage() {
  const router = useRouter();
  const { revalidate } = useAuth();

  useEffect(() => {
    // After SSO redirect, the refresh cookie is set.
    // Revalidate will trigger a refresh flow to get an access token.
    revalidate().then(() => {
      router.replace("/chat");
    });
  }, [revalidate, router]);

  return (
    <div className="flex min-h-screen items-center justify-center">
      <p className="text-text-secondary">Completing sign in...</p>
    </div>
  );
}
