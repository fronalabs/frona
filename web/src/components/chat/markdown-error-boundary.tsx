"use client";

import { Component, type ErrorInfo, type ReactNode } from "react";

interface MarkdownErrorBoundaryProps {
  text: string;
  children: ReactNode;
}

export class MarkdownErrorBoundary extends Component<MarkdownErrorBoundaryProps, { failed: boolean }> {
  state = { failed: false };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error("Markdown rendering failed; displaying plain text.", error, info);
  }

  componentDidUpdate(previous: MarkdownErrorBoundaryProps) {
    if (this.state.failed && previous.text !== this.props.text) {
      this.setState({ failed: false });
    }
  }

  render() {
    return this.state.failed
      ? <div className="whitespace-pre-wrap break-words">{this.props.text}</div>
      : this.props.children;
  }
}
