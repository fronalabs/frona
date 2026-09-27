"use client";

import {
  createContext,
  useContext,
  useState,
  useEffect,
  useCallback,
  createElement,
} from "react";
import { api, setAccessToken, ApiError } from "./api-client";
import type {
  UserInfo,
  AuthResponse,
  LoginRequest,
  RegisterRequest,
  AuthConfig,
} from "./types";

const API_URL = process.env.NEXT_PUBLIC_FRONA_SERVER_BACKEND_URL || "";

/// `offline` (server unreachable, session status unknown) and
/// `unauthenticated` (server confirmed no session) drive different AppGate
/// behavior — don't collapse them into one error state.
export type ConnectionState = "loading" | "online" | "offline" | "unauthenticated";

interface AuthContextValue {
  user: UserInfo | null;
  connectionState: ConnectionState;
  needsSetup: boolean;
  authConfig: AuthConfig | null;
  login: (req: LoginRequest) => Promise<void>;
  register: (req: RegisterRequest) => Promise<void>;
  logout: () => void;
  revalidate: () => Promise<void>;
  initiateSso: () => void;
}

const AuthContext = createContext<AuthContextValue | null>(null);

/// Discriminates real auth failure from transient unreachability so callers
/// can decide between redirect-to-login and offline-retry.
type MeProbe =
  | { kind: "user"; user: UserInfo }
  | { kind: "unauthenticated" }
  | { kind: "unavailable" };

async function probeCurrentUser(): Promise<MeProbe> {
  try {
    const user = await api.get<UserInfo>("/api/auth/me");
    return { kind: "user", user };
  } catch (err: unknown) {
    if (err instanceof ApiError) {
      if (err.kind === "unavailable") return { kind: "unavailable" };
      if (err.status === 401 || err.status === 403) {
        return { kind: "unauthenticated" };
      }
    }
    // Unknown error shape → "unavailable" so a parsing glitch doesn't bounce
    // the user to /login.
    return { kind: "unavailable" };
  }
}

async function fetchAuthConfig(): Promise<AuthConfig | null> {
  try {
    const res = await fetch(`${API_URL}/api/auth/config`);
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  }
}

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<UserInfo | null>(null);
  const [connectionState, setConnectionState] =
    useState<ConnectionState>("loading");
  const [needsSetup, setNeedsSetup] = useState(false);
  const [authConfig, setAuthConfig] = useState<AuthConfig | null>(null);

  const probe = useCallback(async () => {
    const [meRes, cfg] = await Promise.all([
      probeCurrentUser(),
      fetchAuthConfig(),
    ]);
    setAuthConfig(cfg);
    if (meRes.kind === "user") {
      setUser(meRes.user);
      setNeedsSetup(meRes.user.needs_setup === true);
      setConnectionState("online");
    } else if (meRes.kind === "unauthenticated") {
      setUser(null);
      setConnectionState("unauthenticated");
    } else {
      // Keep cached `user` so AppGate's reconnect overlay doesn't drop data.
      setConnectionState("offline");
    }
  }, []);

  useEffect(() => {
    probe();
  }, [probe]);

  const login = useCallback(async (req: LoginRequest) => {
    const res = await api.post<AuthResponse>("/api/auth/login", req);
    if (res.token) {
      setAccessToken(res.token);
    }
    setUser(res.user);
    setConnectionState("online");
  }, []);

  const register = useCallback(async (req: RegisterRequest) => {
    const res = await api.post<AuthResponse>("/api/auth/register", req);
    if (res.token) {
      setAccessToken(res.token);
    }
    setUser(res.user);
    setConnectionState("online");
    const me = await probeCurrentUser();
    if (me.kind === "user" && me.user.needs_setup) {
      setNeedsSetup(true);
    }
  }, []);

  const logout = useCallback(async () => {
    await api.post("/api/auth/logout", {}).catch(() => {});
    setAccessToken(null);
    setUser(null);
    setConnectionState("unauthenticated");
  }, []);

  const revalidate = useCallback(async () => {
    await probe();
  }, [probe]);

  const initiateSso = useCallback(() => {
    // SSO requires a full-page navigation to the backend's authorization endpoint.
    // eslint-disable-next-line @next/next/no-location-assign-relative-destination
    window.location.href = `${API_URL}/api/auth/sso/authorize`;
  }, []);

  return createElement(
    AuthContext.Provider,
    {
      value: {
        user,
        connectionState,
        needsSetup,
        authConfig,
        login,
        register,
        logout,
        revalidate,
        initiateSso,
      },
    },
    children,
  );
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error("useAuth must be used within AuthProvider");
  return ctx;
}
