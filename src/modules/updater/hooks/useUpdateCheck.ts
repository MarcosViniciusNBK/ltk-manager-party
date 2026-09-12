import { useEffect } from "react";

import { useUpdaterStore } from "@/stores";

import { useCheckForUpdate } from "../api";
import { mockUpdate } from "../mockUpdate";

/**
 * Check for an update shortly after the app mounts.
 *
 * A dev run with `VITE_MOCK_UPDATE=1` gets a stand-in update instead, to
 * exercise the titlebar cell and the changelog dialog.
 */
export function useUpdateCheck({ checkOnMount = true, delayMs = 3000 } = {}) {
  const checkForUpdate = useCheckForUpdate();

  useEffect(() => {
    if (import.meta.env.DEV) {
      if (import.meta.env.VITE_MOCK_UPDATE === "1") {
        useUpdaterStore.setState({ update: mockUpdate() });
      }
      return;
    }
    if (!checkOnMount) return;

    let lastCheckAt = Date.now();
    const checkIfAvailable = (force = false) => {
      const state = useUpdaterStore.getState();
      if (state.checking || state.updating || state.update) return;
      if (!force && Date.now() - lastCheckAt < 15 * 60 * 1000) return;
      lastCheckAt = Date.now();
      void checkForUpdate();
    };

    const timeoutId = setTimeout(() => checkIfAvailable(true), delayMs);
    // Keep long-running instances current too. Focus/visibility checks make a resumed laptop react
    // promptly, while the interval is only a defensive fallback and never overlaps another check.
    const intervalId = window.setInterval(() => checkIfAvailable(), 60 * 60 * 1000);
    const checkWhenVisible = () => {
      if (document.visibilityState === "visible") checkIfAvailable();
    };
    const checkOnFocus = () => checkIfAvailable();
    window.addEventListener("focus", checkOnFocus);
    document.addEventListener("visibilitychange", checkWhenVisible);

    return () => {
      clearTimeout(timeoutId);
      window.clearInterval(intervalId);
      window.removeEventListener("focus", checkOnFocus);
      document.removeEventListener("visibilitychange", checkWhenVisible);
    };
  }, [checkOnMount, delayMs, checkForUpdate]);
}
