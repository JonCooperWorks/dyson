/* Dyson — shared JSX bits: icons, small components */

const { useState, useEffect, useRef, useMemo, useLayoutEffect } = React;

// tiny inline SVG icons, monoline, 14px
function Icon({ name, size = 14, style, className }) {
  const common = { width: size, height: size, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor', strokeWidth: 1.5, strokeLinecap: 'round', strokeLinejoin: 'round', style, className };
  switch (name) {
    case 'chat':      return <svg {...common}><path d="M3 4h10v7H6.5L3 13.5V4z"/></svg>;
    case 'brain':     return <svg {...common}><path d="M8 2.5c-1.3 0-2.5.7-3 2-1.3.2-2 1.2-2 2.5 0 1 .5 1.7 1 2 0 1.4 1.2 2.5 2.5 2.5.8 0 1.5-.4 1.5-.4v-8.6z"/><path d="M8 2.5c1.3 0 2.5.7 3 2 1.3.2 2 1.2 2 2.5 0 1-.5 1.7-1 2 0 1.4-1.2 2.5-2.5 2.5-.8 0-1.5-.4-1.5-.4"/></svg>;
    case 'activity':  return <svg {...common}><path d="M1.5 8h3l2-5 3 10 2-5h3"/></svg>;
    case 'plug':      return <svg {...common}><path d="M10 1.5v3M6 1.5v3M5 4.5h6v3a3 3 0 0 1-6 0v-3z"/><path d="M8 10.5v4"/></svg>;
    case 'shield':    return <svg {...common}><path d="M8 1.5 3 3.5v4c0 3 2.5 5.5 5 6.5 2.5-1 5-3.5 5-6.5v-4L8 1.5z"/></svg>;
    case 'plus':      return <svg {...common}><path d="M8 3v10M3 8h10"/></svg>;
    case 'search':    return <svg {...common}><circle cx="7" cy="7" r="4"/><path d="m10 10 3 3"/></svg>;
    case 'send':      return <svg {...common}><path d="M2 8 14 2 10 14 8 9 2 8z"/></svg>;
    case 'paperclip': return <svg {...common}><path d="M12 6.5 7.5 11a2.5 2.5 0 0 1-3.5-3.5l5-5a3 3 0 0 1 4.5 4l-5 5"/></svg>;
    case 'slash':     return <svg {...common}><path d="M10 3 6 13"/></svg>;
    case 'stop':      return <svg {...common}><rect x="4" y="4" width="8" height="8" rx="1"/></svg>;
    case 'copy':      return <svg {...common}><rect x="5" y="5" width="8" height="8" rx="1"/><path d="M3 10V4a1 1 0 0 1 1-1h6"/></svg>;
    case 'fork':      return <svg {...common}><circle cx="4" cy="3" r="1.5"/><circle cx="12" cy="3" r="1.5"/><circle cx="8" cy="13" r="1.5"/><path d="M4 4.5V7a2 2 0 0 0 2 2h4a2 2 0 0 0 2-2V4.5M8 9v2.5"/></svg>;
    case 'star':      return <svg {...common}><path d="m8 2 1.8 3.8 4.2.5-3 2.9.8 4.1L8 11.3l-3.8 2 .8-4.1-3-2.9 4.2-.5L8 2z"/></svg>;
    case 'chev':      return <svg {...common}><path d="m5 4 4 4-4 4"/></svg>;
    case 'chevd':     return <svg {...common}><path d="m4 5 4 4 4-4"/></svg>;
    case 'x':         return <svg {...common}><path d="m4 4 8 8M12 4l-8 8"/></svg>;
    case 'split':     return <svg {...common}><rect x="2" y="3" width="5" height="10" rx="1"/><rect x="9" y="3" width="5" height="10" rx="1"/></svg>;
    case 'tree':      return <svg {...common}><rect x="6" y="1.5" width="4" height="3" rx="0.5"/><rect x="1.5" y="10.5" width="4" height="3" rx="0.5"/><rect x="10.5" y="10.5" width="4" height="3" rx="0.5"/><path d="M8 4.5v3M3.5 7.5h9v3"/></svg>;
    case 'file':      return <svg {...common}><path d="M4 1.5h5l3 3V14a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5V2a.5.5 0 0 1 .5-.5z"/><path d="M9 1.5v3h3"/></svg>;
    case 'folder':    return <svg {...common}><path d="M2 4a1 1 0 0 1 1-1h3l1 1.5h6a1 1 0 0 1 1 1V12a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V4z"/></svg>;
    case 'dot':       return <svg {...common}><circle cx="8" cy="8" r="2" fill="currentColor"/></svg>;
    case 'arr-down':  return <svg {...common}><path d="M8 3v10M4 9l4 4 4-4"/></svg>;
    case 'arr-right': return <svg {...common}><path d="M3 8h10M9 4l4 4-4 4"/></svg>;
    case 'refresh':   return <svg {...common}><path d="M2 8a6 6 0 0 1 10-4.5L14 5M14 8a6 6 0 0 1-10 4.5L2 11"/><path d="M14 2v3h-3M2 14v-3h3"/></svg>;
    case 'rate':      return <svg {...common}><path d="M3 9l2.5 2.5L13 4"/></svg>;
    case 'play':      return <svg {...common}><path d="M5 3.5 12 8l-7 4.5v-9z" fill="currentColor"/></svg>;
    case 'menu':      return <svg {...common}><path d="M2 4h12M2 8h12M2 12h12"/></svg>;
    default: return null;
  }
}

function Chip({ tone = '', children }) {
  return <span className={`chip ${tone}`}>{children}</span>;
}

function Kbd({ children }) { return <span className="kbd">{children}</span>; }

// expose globally
Object.assign(window, { Icon, Chip, Kbd });
