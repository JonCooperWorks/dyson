// Vite entry.  Builds the HTTP + SSE client, boots the reactive store
// from /api/*, and mounts the React tree inside <ApiProvider> so every
// component reaches the client through useApi() — no window globals,
// no CustomEvent channel, no bump() counter.

import './styles/tokens.css';
import './styles/layout.css';
import './styles/turns.css';
import './styles/panels.css';

import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './components/app.jsx';
import { DysonClient } from './api/client.js';
import { boot } from './api/boot.js';
import { ApiProvider } from './hooks/useApi.js';

const client = new DysonClient();
boot(client);

const root = ReactDOM.createRoot(document.getElementById('root'));
root.render(
  <ApiProvider client={client}>
    <App/>
  </ApiProvider>
);
