import React from "react";

// Class component is required here: React error boundaries can ONLY be
// implemented via getDerivedStateFromError / componentDidCatch on a class.
// Wrap each top-level region (active view, side panel, etc.) so a
// render-phase throw in one region (d3-sankey index mismatch, sigma WebGL
// context loss, deck.gl parent display:none, malformed payload) doesn't
// blank-screen the entire SPA.
//
// `region` is shown in the fallback UI to help triage which boundary
// caught the error.

interface ErrorBoundaryProps {
  children: React.ReactNode;
  region?: string;
}

interface ErrorBoundaryState {
  err: Error | null;
}

export class ErrorBoundary extends React.Component<ErrorBoundaryProps, ErrorBoundaryState> {
  constructor(props: ErrorBoundaryProps) {
    super(props);
    this.state = { err: null };
  }

  static getDerivedStateFromError(err: Error): ErrorBoundaryState {
    return { err };
  }

  override componentDidCatch(err: Error, info: React.ErrorInfo): void {
    // Local-only desktop app — log to console for DevTools triage.
    // No telemetry; no exfil.
    // eslint-disable-next-line no-console
    console.error(`[vision:error-boundary${this.props.region ? `:${this.props.region}` : ""}]`, err, info);
  }

  reset = (): void => {
    this.setState({ err: null });
  };

  override render(): React.ReactNode {
    if (this.state.err) {
      const region = this.props.region ?? "view";
      return (
        <div className="vz-view-error" role="alert">
          <strong>{region} crashed:</strong> {this.state.err.message}
          <button
            type="button"
            className="vz-side-close"
            onClick={this.reset}
            aria-label="reset error boundary"
            style={{ marginLeft: "0.5rem" }}
          >
            retry
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
