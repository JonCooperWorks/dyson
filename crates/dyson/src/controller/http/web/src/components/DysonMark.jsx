import React from 'react';

// The Dyson-swarm mark: a flat, single-colour ring of collector panels with
// two lifting off (the swarm) around a solid core. No gradients, glow, or
// strokes — a clean geometric glyph in the spirit of the OpenAI / Anthropic
// marks, tinted the Dyson brand blue by default. Pass `color` to override the
// fill (e.g. "currentColor" or white on a coloured tile).

// Collector panels, origin-centred. The last two are lifted outward at the
// top-right — the swarm breaking away from the shell.
const PANELS = [
  'M56.9,-11.1 L94.2,-18.3 L94.2,18.3 L56.9,11.1Z',
  'M54.8,18.9 L90.8,31.3 L72.5,63.0 L43.8,38.1Z',
  'M38.1,43.8 L63.0,72.5 L31.3,90.8 L18.9,54.8Z',
  'M11.1,56.9 L18.3,94.2 L-18.3,94.2 L-11.1,56.9Z',
  'M-18.9,54.8 L-31.3,90.8 L-63.0,72.5 L-38.1,43.8Z',
  'M-43.8,38.1 L-72.5,63.0 L-90.8,31.3 L-54.8,18.9Z',
  'M-56.9,11.1 L-94.2,18.3 L-94.2,-18.3 L-56.9,-11.1Z',
  'M-54.8,-18.9 L-90.8,-31.3 L-72.5,-63.0 L-43.8,-38.1Z',
  'M-38.1,-43.8 L-63.0,-72.5 L-31.3,-90.8 L-18.9,-54.8Z',
  'M-11.1,-56.9 L-18.3,-94.2 L18.3,-94.2 L11.1,-56.9Z',
  'M22.8,-66.2 L35.2,-102.1 L70.9,-81.5 L45.9,-52.8Z',
  'M52.8,-45.9 L81.5,-70.9 L102.1,-35.2 L66.2,-22.8Z',
];
const SHELL = PANELS.join(' ');

// Default brand blue — reads on both light and dark surfaces.
export const DYSON_BLUE = '#3b82f6';

// The bare mark (shell + core), origin-centred, ready to drop into a viewBox
// or transform.
function Glyph({ color }) {
  return (
    <>
      <path d={SHELL} fill={color}/>
      <circle r="26" fill={color}/>
    </>
  );
}

// Standalone brand mark. Transparent ground; the surface shows through.
export function DysonMark({ size = 24, color = DYSON_BLUE, title = 'Dyson', ...rest }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="-112 -112 224 224"
      role="img"
      aria-label={title}
      style={{ display: 'block' }}
      {...rest}
    >
      <Glyph color={color}/>
    </svg>
  );
}

// The Computer-kind mark: the Dyson mark framed inside a monitor. The monitor
// inherits currentColor (theme ink) so it reads on any card; the mark keeps
// its brand blue.
export function ComputerMark({ size = 24, color = DYSON_BLUE, title = 'Dyson Computer', ...rest }) {
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
      <g transform="translate(120,84) scale(0.62)">
        <Glyph color={color}/>
      </g>
    </svg>
  );
}
