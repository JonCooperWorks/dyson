import React from 'react';

// The Dyson mark: a segmented Dyson-sphere shell of collector panels
// enclosing a bright core, with two panels lifting off as a swarm. Single
// source of truth for the brand mark — used by the topbar, the hire wizard
// (Dyson agent-kind card), and the create-agent page. The favicon/PWA tiles
// live as static SVGs under public/icons and mirror this geometry on a dark
// square.

// Alternating collector panels (blue two-tone) plus the two brighter,
// lifted-off "swarm" panels at the top-right. Origin-centred so the same
// geometry drops into any viewBox.
const PANELS = [
  { f: '#3b82f6', d: 'M56.9,-11.1 L94.2,-18.3 L94.2,18.3 L56.9,11.1Z' },
  { f: '#1d4ed8', d: 'M54.8,18.9 L90.8,31.3 L72.5,63.0 L43.8,38.1Z' },
  { f: '#3b82f6', d: 'M38.1,43.8 L63.0,72.5 L31.3,90.8 L18.9,54.8Z' },
  { f: '#1d4ed8', d: 'M11.1,56.9 L18.3,94.2 L-18.3,94.2 L-11.1,56.9Z' },
  { f: '#3b82f6', d: 'M-18.9,54.8 L-31.3,90.8 L-63.0,72.5 L-38.1,43.8Z' },
  { f: '#1d4ed8', d: 'M-43.8,38.1 L-72.5,63.0 L-90.8,31.3 L-54.8,18.9Z' },
  { f: '#3b82f6', d: 'M-56.9,11.1 L-94.2,18.3 L-94.2,-18.3 L-56.9,-11.1Z' },
  { f: '#1d4ed8', d: 'M-54.8,-18.9 L-90.8,-31.3 L-72.5,-63.0 L-43.8,-38.1Z' },
  { f: '#3b82f6', d: 'M-38.1,-43.8 L-63.0,-72.5 L-31.3,-90.8 L-18.9,-54.8Z' },
  { f: '#1d4ed8', d: 'M-11.1,-56.9 L-18.3,-94.2 L18.3,-94.2 L11.1,-56.9Z' },
  { f: '#38bdf8', d: 'M22.8,-66.2 L35.2,-102.1 L70.9,-81.5 L45.9,-52.8Z' },
  { f: '#38bdf8', d: 'M52.8,-45.9 L81.5,-70.9 L102.1,-35.2 L66.2,-22.8Z' },
];

// The bare shell + core, origin-centred, ready to drop into a transform.
// `orbit` adds the dashed capture-orbit ring + swarm nodes (skip it at small
// sizes where it just muddies). The core is a white→blue radial orb ringed in
// navy with a highlight glint — reads as a glowing energy core on light AND
// dark surfaces (a pale core washes out on white). `cid` scopes the gradient
// id so multiple marks on one page don't collide.
function Sphere({ orbit = false, cid }) {
  const grad = `dyson-core-${cid}`;
  return (
    <>
      <defs>
        <radialGradient id={grad} cx="42%" cy="40%" r="62%">
          <stop offset="0" stopColor="#ffffff"/>
          <stop offset="0.5" stopColor="#93c5fd"/>
          <stop offset="1" stopColor="#2563eb"/>
        </radialGradient>
      </defs>
      {orbit ? (
        <>
          <circle r="128" fill="none" stroke="#3b82f6" strokeWidth="2" strokeDasharray="2 9" opacity="0.5"/>
          <circle cx="0" cy="-128" r="5" fill="#7dd3fc"/>
          <circle cx="112" cy="62" r="4" fill="#7dd3fc"/>
        </>
      ) : null}
      {PANELS.map((p, i) => <path key={i} fill={p.f} d={p.d}/>)}
      <circle r="40" fill={`url(#${grad})`} stroke="#1e40af" strokeWidth="4"/>
      <circle cx="-8" cy="-8" r="7" fill="#ffffff" opacity="0.9"/>
    </>
  );
}

// Standalone brand mark on a transparent ground (the surface shows through).
export function DysonMark({ size = 24, orbit = false, title = 'Dyson', ...rest }) {
  const cid = React.useId();
  const r = orbit ? 132 : 110;
  return (
    <svg
      width={size}
      height={size}
      viewBox={`${-r} ${-r} ${r * 2} ${r * 2}`}
      role="img"
      aria-label={title}
      style={{ display: 'block' }}
      {...rest}
    >
      <Sphere orbit={orbit} cid={cid}/>
    </svg>
  );
}

// The Computer-kind mark: the Dyson sphere framed inside a monitor. The
// monitor inherits currentColor (theme ink) so it reads on any card; the
// sphere keeps its brand blues.
export function ComputerMark({ size = 24, title = 'Dyson Computer', ...rest }) {
  const cid = React.useId();
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 240 224"
      role="img"
      aria-label={title}
      style={{ display: 'block' }}
      {...rest}
    >
      <rect x="16" y="8" width="208" height="152" rx="18" fill="none" stroke="currentColor" strokeWidth="11"/>
      <rect x="108" y="160" width="24" height="26" fill="currentColor"/>
      <rect x="74" y="186" width="92" height="14" rx="7" fill="currentColor"/>
      <g transform="translate(120,84) scale(0.6)">
        <Sphere orbit={false} cid={cid}/>
      </g>
    </svg>
  );
}
