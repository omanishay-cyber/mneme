import { useEffect, useState } from "react";

// First-visit onboarding hint for the Force Galaxy canvas.
//
// First-time users land on a graph canvas with no obvious affordances —
// they don't know they can drag to pan or scroll to zoom. graphify and
// CRG both ship without any hint at all (we lead them on this one).
//
// Behaviour:
//   * On mount, check `localStorage["vz.onboarded"]`.
//   * If absent, render a dismissible glass-style card centered over the
//     canvas with the three primary affordances.
//   * Clicking the close button (or "Got it") sets the flag and unmounts.
//   * Honors `prefers-reduced-motion` via the global CSS rule.
//
// Lives outside ForceGalaxy on purpose so future views can reuse the
// same component with custom copy if needed (passed via the `message`
// prop, defaults to the Force Galaxy text).

const STORAGE_KEY = "vz.onboarded";

export interface OnboardingHintProps {
  /** Optional override; defaults to the Force Galaxy hint copy. */
  message?: string;
}

/**
 * Returns true if the SSR / non-browser environment is detected, so the
 * hint can no-op cleanly during prerender or during tests that don't
 * stub `window`.
 */
function noBrowser(): boolean {
  return typeof window === "undefined" || typeof window.localStorage === "undefined";
}

export function OnboardingHint({
  message = "Drag to pan · Scroll to zoom · Click a node to inspect",
}: OnboardingHintProps): JSX.Element | null {
  const [visible, setVisible] = useState<boolean>(false);

  useEffect(() => {
    if (noBrowser()) return;
    try {
      const seen = window.localStorage.getItem(STORAGE_KEY);
      if (seen === null) setVisible(true);
    } catch {
      // localStorage can throw in private-mode Safari; degrade by not
      // showing the hint rather than crashing the app.
    }
  }, []);

  const dismiss = (): void => {
    setVisible(false);
    if (noBrowser()) return;
    try {
      window.localStorage.setItem(STORAGE_KEY, "1");
    } catch {
      // Same Safari-private-mode caveat — best-effort only.
    }
  };

  if (!visible) return null;

  return (
    <div className="vz-onboarding" role="dialog" aria-label="onboarding hint">
      <p className="vz-onboarding-message">{message}</p>
      <button
        type="button"
        className="vz-onboarding-dismiss"
        onClick={dismiss}
        aria-label="dismiss onboarding hint"
      >
        Got it
      </button>
    </div>
  );
}
