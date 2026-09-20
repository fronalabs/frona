import type { NextConfig } from "next";
import { PHASE_PRODUCTION_BUILD } from "next/constants";

const nextConfig = (phase: string): NextConfig => ({
  // Export into a child directory so Next can recreate it without removing a mount.
  distDir: phase === PHASE_PRODUCTION_BUILD ? "target/out" : ".next",
  output: "export",
  images: {
    unoptimized: true,
  },
  env: {
    NEXT_PUBLIC_FRONA_SERVER_BACKEND_URL:
      process.env.FRONA_SERVER_BACKEND_URL || "",
  },
});

export default nextConfig;
