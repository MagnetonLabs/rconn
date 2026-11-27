// Shared transport for the portal API (contract: Rconn portal API v1).
// Every area module (auth, computers, people, policies, audit, sso, org) goes through `api()`.

export const API_PREFIX = '/api/v1';

export type ErrorCode =
  | 'not_found'
  | 'unauthorized'
  | 'forbidden'
  | 'invalid'
  | 'rate_limited'
  | 'conflict'
  | 'needs_code'
  | 'network';

export class ApiError extends Error {
  readonly status: number;
  readonly code: ErrorCode | string;
  constructor(status: number, code: ErrorCode | string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** Fired on any 401 so the session can drop to the sign-in page. */
export const UNAUTHORIZED_EVENT = 'rconn:unauthorized';

type Method = 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';

export type Query = Record<string, string | number | boolean | null | undefined>;

export function withQuery(path: string, query?: Query): string {
  if (!query) return path;
  const params = new URLSearchParams();
  for (const [k, v] of Object.entries(query)) {
    if (v === undefined || v === null || v === '') continue;
    params.set(k, String(v));
  }
  const s = params.toString();
  return s ? `${path}?${s}` : path;
}

export function url(path: string, query?: Query): string {
  return withQuery(API_PREFIX + path, query);
}

/**
 * Calls the API. Non-GET requests always send `Content-Type: application/json`; POST, PUT and
 * PATCH always carry a JSON body (`{}` when the endpoint takes none). The browser adds `Origin`.
 * Resolves to `undefined` for 202/204 or an empty body.
 */
export async function api<T>(method: Method, path: string, body?: unknown, query?: Query): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' };
  const init: RequestInit = { method, headers, credentials: 'same-origin' };
  if (method !== 'GET') {
    headers['Content-Type'] = 'application/json';
    if (method !== 'DELETE') init.body = JSON.stringify(body ?? {});
  }
  let res: Response;
  try {
    res = await fetch(url(path, query), init);
  } catch {
    throw new ApiError(0, 'network', 'Could not reach the server. Check your connection and try again.');
  }
  if (res.status === 401 && typeof window !== 'undefined' && !path.startsWith('/auth/')) {
    window.dispatchEvent(new Event(UNAUTHORIZED_EVENT));
  }
  const text = res.status === 204 ? '' : await res.text();
  let data: unknown = undefined;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = undefined;
    }
  }
  if (!res.ok) {
    const err = (data as { error?: { code?: string; message?: string } } | undefined)?.error;
    throw new ApiError(res.status, err?.code ?? statusCode(res.status), err?.message ?? defaultMessage(res.status));
  }
  return data as T;
}

function statusCode(status: number): ErrorCode {
  switch (status) {
    case 400:
    case 422:
      return 'invalid';
    case 401:
      return 'unauthorized';
    case 403:
      return 'forbidden';
    case 404:
      return 'not_found';
    case 409:
      return 'conflict';
    case 429:
      return 'rate_limited';
    default:
      return 'invalid';
  }
}

function defaultMessage(status: number): string {
  switch (status) {
    case 401:
      return 'Your session ended. Sign in again.';
    case 403:
      return 'Your role does not allow this.';
    case 404:
      return 'Not found.';
    case 409:
      return 'That conflicts with the current state. Reload and try again.';
    case 429:
      return 'Too many attempts. Wait a minute and try again.';
    default:
      return status >= 500 ? 'The server had a problem. Try again.' : 'The request was not accepted.';
  }
}

/** Org-scoped path helper: `/orgs/{org_id}/…`. */
export function orgPath(orgId: string, rest: string): string {
  return `/orgs/${encodeURIComponent(orgId)}${rest}`;
}
