import { describe, expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const indexHtml = readFileSync(join(process.cwd(), 'index.html'), 'utf8');
const swarmThemeCss = readFileSync(join(process.cwd(), 'src/styles/swarm-theme.css'), 'utf8');
const turnsCss = readFileSync(join(process.cwd(), 'src/styles/turns.css'), 'utf8');

function blockFor(selector, css) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = css.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`));
  return match?.[1] || '';
}

describe('mobile form controls', () => {
  test('keeps viewport zoom accessible', () => {
    expect(indexHtml).toContain('<meta name="viewport" content="width=device-width, initial-scale=1"/>');
    expect(indexHtml).not.toMatch(/maximum-scale|user-scalable/);
  });

  test('keeps mobile inputs at 16px or larger so iOS does not zoom on focus', () => {
    expect(swarmThemeCss).toMatch(/@media \(max-width: 700px\)[\s\S]*select[\s\S]*textarea[\s\S]*font-size:\s*16px\s*!important/);
  });

  test('pins the chat composer textarea at the iOS focus-zoom threshold', () => {
    expect(blockFor('.composer textarea', turnsCss)).toMatch(/font-size:\s*16px\b/);
  });
});
