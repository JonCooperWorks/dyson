// Vite entry.  Auth bootstrap runs first (so an OIDC redirect happens
// before we paint anything), then we build the HTTP + SSE client with
// a token-supplier closure, kick off the cold-load + poll, and mount
// React inside <ApiProvider>.  No window globals, no CustomEvent
// channel, no bump() counter.

import './styles/tokens.css';
import './styles/layout.css';
import './styles/turns.css';
import './styles/panels.css';

import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './components/app.jsx';
import { DysonClient } from './api/client.js';
import { boot } from './api/boot.js';
import { bootstrapAuth } from './api/auth.js';
import { ApiProvider } from './hooks/useApi.js';

bootstrapAuth().then(session => {
  // `getToken` is read fresh on every request through DysonClient,
  // so the silent-refresh path inside auth.js doesn't have to reach
  // back here when it rotates the access token.
  const client = new DysonClient({ getToken: session.getToken });
  boot(client);

  const root = ReactDOM.createRoot(document.getElementById('root'));
  root.render(
    <ApiProvider client={client}>
      <App/>
    </ApiProvider>
  );
}).catch(err => {
  // Discovery / token exchange failed — paint a small error frame
  // instead of leaving the page blank.  The console carries the
  // actual error so an operator can grep for it; the frame just
  // tells the user something is up.
  console.error('[dyson] auth bootstrap failed', err);
  const root = document.getElementById('root');
  if (root) {
    root.innerHTML = `<div style="padding:32px;font-family:system-ui;color:#eee;">
      <h2>Sign-in failed</h2>
      <p>${String(err.message || err)}</p>
      <p>Check that your IdP issuer is reachable and the client_id is registered.</p>
    </div>`;
  }
});
