import { useCallback, useEffect, useRef, useState } from 'react';

export interface Resource<T> {
  data: T | undefined;
  error: unknown;
  loading: boolean;
  reload: () => void;
  setData: (fn: (prev: T | undefined) => T | undefined) => void;
}

/** Loads `fn()` when `deps` change. Ignores results from superseded calls. */
export function useResource<T>(fn: () => Promise<T>, deps: unknown[]): Resource<T> {
  const [data, setDataState] = useState<T | undefined>(undefined);
  const [error, setError] = useState<unknown>(undefined);
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  const seq = useRef(0);

  useEffect(() => {
    const my = ++seq.current;
    setLoading(true);
    setError(undefined);
    fn().then(
      (d) => {
        if (my !== seq.current) return;
        setDataState(d);
        setLoading(false);
      },
      (e) => {
        if (my !== seq.current) return;
        setError(e);
        setLoading(false);
      },
    );
  }, [...deps, tick]);

  const reload = useCallback(() => setTick((t) => t + 1), []);
  const setData = useCallback((f: (prev: T | undefined) => T | undefined) => setDataState(f), []);
  return { data, error, loading, reload, setData };
}
