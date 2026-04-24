// Tests for the clipboard helper.
//
// Two paths worth covering: the happy modern-API path, and the legacy
// textarea-execCommand fallback that kicks in on plain HTTP where
// `navigator.clipboard` isn't defined.  The fallback is the whole
// reason this helper exists — Dyson gets hit over HTTP from the
// Tailscale address and the modern API is gated on a secure context.

import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest';
import { copyToClipboard } from '../lib/clipboard.js';

describe('copyToClipboard', () => {
  let originalClipboard;
  let originalExecCommand;

  beforeEach(() => {
    originalClipboard = Object.getOwnPropertyDescriptor(Navigator.prototype, 'clipboard');
    originalExecCommand = document.execCommand;
  });

  afterEach(() => {
    if (originalClipboard) {
      Object.defineProperty(Navigator.prototype, 'clipboard', originalClipboard);
    } else {
      delete Navigator.prototype.clipboard;
    }
    document.execCommand = originalExecCommand;
  });

  it('returns false on empty input without calling the clipboard API', async () => {
    const writeText = vi.fn();
    Object.defineProperty(Navigator.prototype, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    expect(await copyToClipboard('')).toBe(false);
    expect(await copyToClipboard(null)).toBe(false);
    expect(await copyToClipboard(undefined)).toBe(false);
    expect(writeText).not.toHaveBeenCalled();
  });

  it('uses the modern clipboard API when available', async () => {
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(Navigator.prototype, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    expect(await copyToClipboard('hello')).toBe(true);
    expect(writeText).toHaveBeenCalledWith('hello');
  });

  it('falls back to execCommand when navigator.clipboard is missing', async () => {
    // Delete the property on the prototype so `navigator.clipboard`
    // reads as undefined — this is the plain-HTTP case.
    delete Navigator.prototype.clipboard;
    const execCommand = vi.fn(() => true);
    document.execCommand = execCommand;
    expect(await copyToClipboard('fallback text')).toBe(true);
    expect(execCommand).toHaveBeenCalledWith('copy');
  });

  it('returns false when the modern API rejects', async () => {
    const writeText = vi.fn(() => Promise.reject(new Error('denied')));
    Object.defineProperty(Navigator.prototype, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    expect(await copyToClipboard('x')).toBe(false);
  });
});
