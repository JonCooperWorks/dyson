import { describe, expect, it } from 'vitest';
import { keyboardInsetForVisualViewport } from '../components/app.jsx';

describe('keyboardInsetForVisualViewport', () => {
  it('does not offset the composer when the user is not editing', () => {
    expect(keyboardInsetForVisualViewport({
      editing: false,
      innerHeight: 800,
      visualViewportHeight: 500,
    })).toBe(0);
  });

  it('ignores visual viewport shrinkage caused by WebKit page scale', () => {
    expect(keyboardInsetForVisualViewport({
      editing: true,
      innerHeight: 800,
      visualViewportHeight: 500,
      visualViewportScale: 1.25,
      appHeight: 800,
    })).toBe(0);
  });

  it('does not double-apply the keyboard inset when 100dvh already resized the app', () => {
    expect(keyboardInsetForVisualViewport({
      editing: true,
      innerHeight: 800,
      visualViewportHeight: 504,
      visualViewportScale: 1,
      appHeight: 500,
    })).toBe(0);
  });

  it('keeps the legacy keyboard offset when the app is still layout-viewport height', () => {
    expect(keyboardInsetForVisualViewport({
      editing: true,
      innerHeight: 800,
      visualViewportHeight: 500,
      visualViewportOffsetTop: 10,
      visualViewportScale: 1,
      appHeight: 800,
    })).toBe(290);
  });
});
