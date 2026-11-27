import { useEffect, useRef } from 'react';

/** What {@link poll} needs from the browser (tests pass their own). */
export interface PollEnv {
  doc: Pick<Document, 'visibilityState' | 'addEventListener' | 'removeEventListener'>;
  setInterval: (fn: () => void, ms: number) => unknown;
  clearInterval: (id: unknown) => void;
}

const browserEnv = (): PollEnv => ({
  doc: document,
  setInterval: (fn, ms) => window.setInterval(fn, ms),
  clearInterval: (id) => window.clearInterval(id as number),
});

/**
 * Calls `tick` every `ms` while the page is visible. Hidden: stops; visible again: ticks at
 * once, then every `ms`. Returns the function that stops it for good.
 */
export function poll(tick: () => void, ms: number, env: PollEnv = browserEnv()): () => void {
  let timer: unknown = null;
  const start = () => {
    if (timer === null) timer = env.setInterval(tick, ms);
  };
  const stop = () => {
    if (timer !== null) env.clearInterval(timer);
    timer = null;
  };
  const onVisibility = () => {
    if (env.doc.visibilityState === 'visible') {
      if (timer === null) tick();
      start();
    } else {
      stop();
    }
  };
  env.doc.addEventListener('visibilitychange', onVisibility);
  if (env.doc.visibilityState === 'visible') start();
  return () => {
    stop();
    env.doc.removeEventListener('visibilitychange', onVisibility);
  };
}

/** {@link poll} for a component's lifetime; `tick` may change between renders. */
export function usePoll(tick: () => void, ms: number) {
  const latest = useRef(tick);
  latest.current = tick;
  useEffect(() => poll(() => latest.current(), ms), [ms]);
}
