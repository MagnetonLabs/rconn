// Replaces fetch for /api/v1 with the in-memory mock server. The session "cookie" lives in the
// given store (localStorage in the browser, a plain variable in tests).

import { API_PREFIX } from '../api/http';
import type { MockServer } from './server';

export interface SessionStore {
  get(): string | null;
  set(v: string | null): void;
}

export function memoryStore(): SessionStore {
  let v: string | null = null;
  return { get: () => v, set: (x) => void (v = x) };
}

export function installMockFetch(server: MockServer, store: SessionStore, opts: { latencyMs?: number } = {}) {
  const realFetch = globalThis.fetch?.bind(globalThis);
  const mockFetch = async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    const u = new URL(raw, 'http://mock.local');
    if (!u.pathname.startsWith(API_PREFIX + '/')) {
      if (!realFetch) throw new TypeError('fetch unavailable');
      return realFetch(input, init);
    }
    const method = (init?.method ?? 'GET').toUpperCase();
    let body: unknown = undefined;
    if (typeof init?.body === 'string' && init.body.length) {
      try {
        body = JSON.parse(init.body);
      } catch {
        return json(400, { error: { code: 'invalid', message: 'Malformed JSON.' } });
      }
    }
    if (opts.latencyMs) await new Promise((r) => setTimeout(r, opts.latencyMs));
    const res = server.handle({
      method,
      path: u.pathname.slice(API_PREFIX.length),
      query: u.searchParams,
      body,
      session: store.get(),
    });
    if (res.setSession !== undefined) store.set(res.setSession);
    if (res.text !== undefined) {
      return new Response(res.text, { status: res.status, headers: { 'Content-Type': res.contentType ?? 'text/plain' } });
    }
    if (res.json === undefined) return new Response(null, { status: res.status });
    return json(res.status, res.json);
  };
  globalThis.fetch = mockFetch as typeof fetch;
  return () => {
    if (realFetch) globalThis.fetch = realFetch;
  };
}

function json(status: number, data: unknown) {
  return new Response(JSON.stringify(data), { status, headers: { 'Content-Type': 'application/json' } });
}
