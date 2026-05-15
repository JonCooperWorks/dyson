import { describe, expect, test } from 'vitest';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const styles = [
  ['layout.css', readFileSync(join(process.cwd(), 'src/styles/layout.css'), 'utf8')],
  ['swarm-theme.css', readFileSync(join(process.cwd(), 'src/styles/swarm-theme.css'), 'utf8')],
];

describe('mobile chat scroll containment', () => {
  test.each(styles)('%s locks the root scroller on mobile', (_name, css) => {
    expect(css).toMatch(/@media \(max-width: 760px\)[\s\S]*html,\s*body\s*\{[\s\S]*min-height:\s*0;[\s\S]*overflow:\s*hidden;[\s\S]*overscroll-behavior:\s*none;/);
    expect(css).toMatch(/@media \(max-width: 760px\)[\s\S]*#root,\s*\.app\s*\{[\s\S]*min-height:\s*0;[\s\S]*overflow:\s*hidden;/);
  });
});
