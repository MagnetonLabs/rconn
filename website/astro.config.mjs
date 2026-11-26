import { defineConfig } from 'astro/config';

export default defineConfig({
  site: 'https://rconn.app',
  trailingSlash: 'ignore',
  build: { format: 'directory', inlineStylesheets: 'always' },
  compressHTML: true,
});
