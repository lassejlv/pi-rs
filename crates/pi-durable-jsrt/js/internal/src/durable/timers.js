// packages/durable/src/timers.ts: the default `HarnessOptions.setTimer` (`setTimeout`), which the
// native Session and Harness arm their timers through, so a host's fake timers see them.

/** Longest delay `setTimeout` supports; a longer one is waited for in several steps. */
const MAX_TIMER_DELAY = 2_147_483_647;

/** Cloudflare Workers: a pending timer keeps a Durable Object from hibernating or being evicted. */
const inWorkers = typeof navigator !== "undefined" && navigator.userAgent === "Cloudflare-Workers";

/**
 * The default `HarnessOptions.setTimer`: `setTimeout`, waiting out delays beyond its limit in steps. A background timer
 * is unreferenced, so it never keeps the process alive. It is declined in Cloudflare Workers and wherever timers cannot
 * be unreferenced.
 */
export const nativeTimer = (callback, delayMs, options) => {
  if (options.background && inWorkers) return undefined;
  const arm = (remaining) => {
    const next = setTimeout(
      () => {
        if (remaining > MAX_TIMER_DELAY) handle = arm(remaining - MAX_TIMER_DELAY);
        else callback();
      },
      Math.min(remaining, MAX_TIMER_DELAY),
    );
    if (options.background) next.unref?.();
    return next;
  };
  let handle = arm(Math.max(0, delayMs));
  if (options.background && typeof handle.unref !== "function") {
    clearTimeout(handle);
    return undefined;
  }
  return () => clearTimeout(handle);
};

/** Wrap a host `setTimer` (see upstream `hostTimers`). */
export function hostTimers(setTimer, report) {
  const arm = setTimer ?? nativeTimer;
  let reported = false;
  const fallBack = (error) => {
    if (reported) return;
    reported = true;
    report(error);
  };
  return {
    foreground: (callback, delayMs) => {
      try {
        const disarm = arm(callback, delayMs, { background: false });
        if (disarm !== undefined) return disarm;
        fallBack(new Error("HarnessOptions.setTimer declined a timer that paces work; using setTimeout"));
      } catch (error) {
        fallBack(error);
      }
      return nativeTimer(callback, delayMs, { background: false });
    },
    background: (callback, delayMs) => {
      try {
        return arm(callback, delayMs, { background: true });
      } catch (error) {
        report(error);
        return undefined;
      }
    },
  };
}
