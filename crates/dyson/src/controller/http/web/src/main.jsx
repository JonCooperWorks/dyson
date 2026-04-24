// Vite entry — bootstraps styles, seeds window.DYSON_DATA, wires the
// live API bridge, then mounts the React tree. bridge.js still probes
// /api/conversations on load; the React tree reads from window.DYSON_DATA
// and re-renders on 'dyson:live-update' events, same as before the
// bundler migration.

import './styles/tokens.css';
import './styles/layout.css';
import './styles/turns.css';
import './styles/panels.css';

import './data.js';
import './bridge.js';

import React from 'react';
import ReactDOM from 'react-dom/client';
import { App } from './components/app.jsx';

const root = ReactDOM.createRoot(document.getElementById('root'));
root.render(<App />);
