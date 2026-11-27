// Browser entry for `npm run dev:mock`. State survives reloads within the tab (sessionStorage);
// `?mock-reset` starts over. Sent emails are on `window.__rconnOutbox` and logged to the console.

import { createMockServer } from './server';
import { installMockFetch } from './install';
import { seed, type Db } from './seed';

const DB_KEY = 'rconn-mock-db';
const SID_KEY = 'rconn-mock-session';

declare global {
  interface Window {
    __rconnOutbox?: Db['outbox'];
    __rconnMock?: { db: Db };
  }
}

function load(): Db {
  try {
    if (new URLSearchParams(location.search).has('mock-reset')) {
      sessionStorage.removeItem(DB_KEY);
      localStorage.removeItem(SID_KEY);
    }
    const raw = sessionStorage.getItem(DB_KEY);
    if (raw) return JSON.parse(raw) as Db;
  } catch {
    /* fall through to a fresh seed */
  }
  return seed();
}

export function startMock() {
  const db = load();
  const server = createMockServer(db);
  const store = {
    get: () => {
      try {
        return localStorage.getItem(SID_KEY);
      } catch {
        return null;
      }
    },
    set: (v: string | null) => {
      try {
        if (v) localStorage.setItem(SID_KEY, v);
        else localStorage.removeItem(SID_KEY);
      } catch {
        /* ignore */
      }
    },
  };
  let mailSeen = db.outbox.length;
  const persist = () => {
    try {
      sessionStorage.setItem(DB_KEY, JSON.stringify(db));
    } catch {
      /* ignore */
    }
    for (; mailSeen < db.outbox.length; mailSeen++) {
      const m = db.outbox[mailSeen];
      console.info(`[mock mail] to ${m.to}: ${m.subject}\n${location.origin}${m.link}${m.code ? `\ncode ${m.code}` : ''}`);
    }
  };
  const handle = server.handle;
  server.handle = (req) => {
    const res = handle(req);
    if (req.method !== 'GET') persist();
    return res;
  };
  installMockFetch(server, store, { latencyMs: 120 });
  window.__rconnOutbox = db.outbox;
  window.__rconnMock = { db };
  persist();
}
