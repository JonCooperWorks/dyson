/* @vitest-environment jsdom */
import { describe, it, expect } from 'vitest';
import { renderBootError } from '../api/boot-error.js';

describe('renderBootError (SEC-04)', () => {
  it('renders the error message as text, never as markup', () => {
    const root = document.createElement('div');
    const payload = '<img src=x onerror="window.__pwned=1">';
    renderBootError(root, new Error(payload));
    expect(root.querySelector('img')).toBeNull();
    expect(root.textContent).toContain(payload);
    expect(root.textContent).toContain('Sign-in failed');
  });

  it('accepts non-Error rejections and a missing root', () => {
    const root = document.createElement('div');
    renderBootError(root, '<b>boom</b>');
    expect(root.querySelector('b')).toBeNull();
    expect(root.textContent).toContain('<b>boom</b>');
    expect(() => renderBootError(null, new Error('x'))).not.toThrow();
  });
});
