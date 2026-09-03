"use client";

import {
  Component,
  createContext,
  createElement,
  useContext,
  type ErrorInfo,
  type ReactNode,
} from "react";
import { DefaultView } from "./default";
import type { ToolView, ToolViewProps } from "./types";

const GenericViewContext = createContext<ReactNode>(null);

/** Signals that the tool data does not match the custom view. */
export function ToolViewFallback() {
  const fallback = useContext(GenericViewContext);
  if (fallback === null) {
    throw new Error("ToolViewFallback must be rendered inside SafeToolView");
  }
  return fallback;
}

/**
 * A failed or cancelled call can return an error payload that does not match
 * the tool's success schema. Keep the known tool view so ToolRow can render
 * its error state without exposing the generic arguments block.
 */
export function shouldUseToolViewFallback(
  result: unknown,
  matchesView: boolean,
  status: ToolViewProps["status"],
): boolean {
  return result !== undefined && !matchesView && status?.type !== "incomplete";
}

interface ErrorBoundaryProps {
  children: ReactNode;
  fallback: ReactNode;
  resetKeys: readonly unknown[];
}

interface ErrorBoundaryState {
  failed: boolean;
}

function resetKeysChanged(
  previous: readonly unknown[],
  next: readonly unknown[],
): boolean {
  return (
    previous.length !== next.length ||
    previous.some((value, index) => !Object.is(value, next[index]))
  );
}

class ToolViewErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { failed: false };

  static getDerivedStateFromError(): ErrorBoundaryState {
    return { failed: true };
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error("Custom tool view failed; using the generic view.", error, info);
  }

  componentDidUpdate(previous: ErrorBoundaryProps) {
    if (this.state.failed && resetKeysChanged(previous.resetKeys, this.props.resetKeys)) {
      this.setState({ failed: false });
    }
  }

  render() {
    return this.state.failed ? this.props.fallback : this.props.children;
  }
}

type SafeToolViewProps = ToolViewProps & {
  view: ToolView;
};

export function SafeToolView({ view, ...props }: SafeToolViewProps) {
  if (view === DefaultView) {
    return <DefaultView {...props} />;
  }

  const fallback = <DefaultView {...props} />;
  const resetKeys = [
    view,
    props.toolCallId,
    props.args,
    props.argsText,
    props.result,
    props.status,
  ];

  return (
    <GenericViewContext.Provider value={fallback}>
      <ToolViewErrorBoundary fallback={fallback} resetKeys={resetKeys}>
        {createElement(view, props)}
      </ToolViewErrorBoundary>
    </GenericViewContext.Provider>
  );
}
