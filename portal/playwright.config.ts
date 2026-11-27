import { defineConfig } from '@playwright/test';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

// Use PW_CHROMIUM if set; otherwise Playwright's own browser, or any cached headless shell when
// the exact revision isn't downloaded (keeps `npx playwright install` optional).
function chromium(): string | undefined {
  if (process.env.PW_CHROMIUM) return process.env.PW_CHROMIUM;
  const cache = join(homedir(), 'Library/Caches/ms-playwright');
  if (!existsSync(cache)) return undefined;
  try {
    const want = JSON.parse(readFileSync('node_modules/playwright-core/browsers.json', 'utf8')).browsers.find(
      (b: { name: string }) => b.name === 'chromium-headless-shell',
    )?.revision;
    if (want && existsSync(join(cache, `chromium_headless_shell-${want}`))) return undefined;
  } catch {
    /* fall back to whatever is cached */
  }
  const shells = readdirSync(cache)
    .filter((d) => d.startsWith('chromium_headless_shell-'))
    .sort()
    .reverse();
  for (const d of shells) {
    for (const arch of ['chrome-headless-shell-mac-arm64', 'chrome-headless-shell-mac-x64']) {
      const p = join(cache, d, arch, 'chrome-headless-shell');
      if (existsSync(p)) return p;
    }
  }
  return undefined;
}

const PORT = 5180;

export default defineConfig({
  testDir: 'e2e',
  fullyParallel: false,
  workers: 1,
  retries: 0,
  timeout: 30_000,
  reporter: [['list']],
  use: {
    baseURL: `http://127.0.0.1:${PORT}/admin/`,
    viewport: { width: 1440, height: 900 },
    launchOptions: { executablePath: chromium() },
    acceptDownloads: true,
    trace: 'off',
  },
  webServer: {
    command: `npx vite --mode mock --host 127.0.0.1 --port ${PORT}`,
    url: `http://127.0.0.1:${PORT}/admin/`,
    reuseExistingServer: true,
    timeout: 60_000,
  },
});
