import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource-variable/schibsted-grotesk';
import './styles.css';
import App from './App';

async function boot() {
  // Replaced at build time: the mock never ships in the production bundle.
  if (import.meta.env.MODE === 'mock') {
    const { startMock } = await import('./mock/browser');
    startMock();
  }
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}

void boot();
