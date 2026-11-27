import { useCallback, useEffect, useRef, useState } from 'react';

export interface Resource<T> {
  data: T | undefined;
  error: unknown;
  loading: boolean;
  reload: () => void;
  /** Re-fetch quietly: no loading state, the current data stays until the new data
   * arrives, and a failure keeps it (the next refresh tries again). Skipped while a load
   * is under way. */
  refresh: () => void;
  setData: (fn: (prev: T | undefined) => T | undefined) => void;
}

/** Loads `fn()` when `deps` change. Ignores results from superseded calls. */
export function useResource<T>(fn: () => Promise<T>, deps: unknown[]): Resource<T> {
  const [data, setDataState] = useState<T | undefined>(undefined);
  const [error, setError] = useState<unknown>(undefined);
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  const seq = useRef(0);
  const busy = useRef(false);
  const latest = useRef(fn);
  latest.current = fn;

  useEffect(() => {
    const my = ++seq.current;
    busy.current = true;
    setLoading(true);
    setError(undefined);
    fn().then(
      (d) => {
        if (my !== seq.current) return;
        busy.current = false;
        setDataState(d);
        setLoading(false);
      },
      (e) => {
        if (my !== seq.current) return;
        busy.current = false;
        setError(e);
        setLoading(false);
      },
    );
  }, [...deps, tick]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  // A refresh applies only if nothing changed meanwhile: no new load (seq) and no local
  // edit through setData (edits), so it can't put back what an edit just replaced.
  const edits = useRef(0);
  const refreshing = useRef(false);
  const refresh = useCallback(() => {
    if (busy.current || refreshing.current) return;
    const [s, e] = [seq.current, edits.current];
    refreshing.current = true;
    const fresh = () => seq.current === s && edits.current === e;
    latest.current().then(
      (d) => {
        refreshing.current = false;
        if (!fresh()) return;
        setDataState(d);
        setError(undefined);
      },
      () => {
        refreshing.current = false;
      },
    );
  }, []);
  const setData = useCallback((f: (prev: T | undefined) => T | undefined) => {
    edits.current++;
    setDataState(f);
  }, []);
  return { data, error, loading, reload, refresh, setData };
}
