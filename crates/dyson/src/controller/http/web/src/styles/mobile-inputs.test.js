import { describe, expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { composerLockedViewportContent } from '../components/turns.jsx';

const indexHtml = readFileSync(join(process.cwd(), 'index.html'), 'utf8');
const swarmThemeCss = readFileSync(join(process.cwd(), 'src/styles/swarm-theme.css'), 'utf8');
const turnsCss = readFileSync(join(process.cwd(), 'src/styles/turns.css'), 'utf8');

describe('mobile form controls', () => {
  test('bakes the iOS focus-zoom lock into the base viewport meta', () => {
    expect(indexHtml).toContain('<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1"/>');
    expect(indexHtml).not.toMatch(/user-scalable/);
  });

  test('keeps the composer focus-time viewport lock in place', () => {
    expect(composerLockedViewportContent('width=device-width, initial-scale=1'))
      .toContain('maximum-scale=1');
    expect(composerLockedViewportContent('width=device-width, initial-scale=1, maximum-scale=2, user-scalable=no'))
      .toBe('width=device-width, initial-scale=1, maximum-scale=1');
  });

  test('keeps mobile inputs at 16px or larger so iOS does not zoom on focus', () => {
    expect(swarmThemeCss).toMatch(/@media \(max-width: 760px\)[\s\S]*input:not\(\[type\]\)[\s\S]*select[\s\S]*textarea[\s\S]*font-size:\s*16px\s*!important/);
    expect(mobileBlock(swarmThemeCss)).not.toContain(':where(');
  });

  test('pins the chat composer textarea above the iOS focus-zoom threshold on mobile', () => {
    expect(turnsCss).toMatch(/\.composer textarea,\s*\.composer-input\s*\{[\s\S]*font-size:\s*16px\s*!important/);
    expect(turnsCss).toMatch(/\.composer textarea,\s*\.composer-input\s*\{[\s\S]*-webkit-text-size-adjust:\s*100%/);
    expect(turnsCss).toMatch(/@media \(max-width: 760px\)\s*\{[\s\S]*\.composer textarea,\s*\.composer-input\s*\{[\s\S]*font-size:\s*17px\s*!important/);
    expect(swarmThemeCss).toMatch(/@media \(max-width: 760px\)\s*\{[\s\S]*\.composer textarea,\s*\.composer-input\s*\{[\s\S]*font-size:\s*17px\s*!important/);
  });
});

function mobileBlock(css) {
  const match = css.match(/@media \(max-width: 760px\)\s*\{([\s\S]*?)\n\}/);
  return match?.[1] || '';
}
