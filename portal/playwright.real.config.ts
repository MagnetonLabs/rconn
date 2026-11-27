import { defineConfig } from '@playwright/test';
import base from './playwright.config';

// Against a real rconn-signaling started by scripts/portal-e2e-real.sh (it passes the
// server's address and the paths the spec needs in RCONN_E2E_* variables).
const BASE = process.env.RCONN_E2E_BASE;
if (!BASE) throw new Error('RCONN_E2E_BASE is not set: run scripts/portal-e2e-real.sh');

export default defineConfig({
  ...base,
  testDir: 'e2e-real',
  timeout: 60_000,
  use: { ...base.use, baseURL: `${BASE}/admin/` },
  webServer: undefined,
});
