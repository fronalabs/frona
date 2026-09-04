import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "export",
  images: {
    unoptimized: true,
  },
  env: {
    NEXT_PUBLIC_FRONA_SERVER_BACKEND_URL:
      process.env.FRONA_SERVER_BACKEND_URL || "",
  },
};

export default nextConfig;
