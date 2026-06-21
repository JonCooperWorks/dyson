import React from 'react';
import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, fireEvent, createEvent, cleanup } from '@testing-library/react';
import {
  Composer,
  clipboardImageFiles,
  composerLockedViewportContent,
  pinComposerFocusGuard,
  prepareComposerFocus,
  slashCommandPreview,
} from '../components/turns.jsx';

const originalMatchMedia = window.matchMedia;
const originalRequestAnimationFrame = window.requestAnimationFrame;
const originalScrollTo = window.scrollTo;

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  document.head.querySelectorAll('meta[name="viewport"][data-test-composer]').forEach(el => el.remove());
  if (originalMatchMedia) {
    window.matchMedia = originalMatchMedia;
  } else {
    delete window.matchMedia;
  }
  if (originalRequestAnimationFrame) {
    window.requestAnimationFrame = originalRequestAnimationFrame;
  } else {
    delete window.requestAnimationFrame;
  }
  if (originalScrollTo) {
    window.scrollTo = originalScrollTo;
  } else {
    delete window.scrollTo;
  }
});

function mockMobile(matches) {
  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    writable: true,
    value: vi.fn(query => ({
      matches: query === '(max-width: 760px)' ? matches : false,
      media: query,
      onchange: null,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      addListener: vi.fn(),
      removeListener: vi.fn(),
      dispatchEvent: vi.fn(),
    })),
  });
}

function mockViewportScrollReset() {
  window.scrollTo = vi.fn();
  window.requestAnimationFrame = vi.fn(cb => {
    cb();
    return 1;
  });
}

describe('Composer image paste', () => {
  it('builds previews for exact, partial, and unknown slash commands', () => {
    const commands = [
      { cmd: '/skill-echo', desc: 'Echo skill', src: 'skill', tool: 'skill_echo' },
      { cmd: '/model', desc: 'Switch model', src: 'controller' },
      { cmd: '/models', desc: 'List models', src: 'controller' },
    ];

    expect(slashCommandPreview('/skill-echo hello', commands)).toMatchObject({
      state: 'exact',
      cmd: '/skill-echo',
      tool: 'skill_echo',
      raw: 'hello',
    });
    expect(slashCommandPreview('/skill-e', commands)).toMatchObject({
      state: 'partial',
      cmd: '/skill-echo',
    });
    expect(slashCommandPreview('/mo', commands)).toMatchObject({
      state: 'partial',
      meta: '2 matches',
    });
    expect(slashCommandPreview('/missing', commands)).toMatchObject({
      state: 'unknown',
      cmd: '/missing',
    });
  });

  it('shows a persistent slash command preview once arguments are being typed', () => {
    const { container, getByTestId, queryByText } = render(
      <Composer
        onSend={() => {}}
        onCancel={() => {}}
        running={false}
        slashCommands={[
          { cmd: '/skill-echo', desc: 'Echo skill', src: 'skill', tool: 'skill_echo' },
        ]}/>
    );
    const textarea = container.querySelector('textarea');

    fireEvent.change(textarea, { target: { value: '/skill-echo hello there' } });

    const preview = getByTestId('slash-preview');
    expect(preview.textContent).toContain('/skill-echo');
    expect(preview.textContent).toContain('Echo skill');
    expect(preview.textContent).toContain('direct tool: skill_echo');
    expect(preview.textContent).toContain('hello there');
    expect(queryByText('skill')).toBeTruthy();
  });

  it('pins the mobile composer textarea above the iOS focus-zoom threshold before focus sampling', () => {
    mockMobile(true);
    mockViewportScrollReset();
    const { container } = render(
      <Composer onSend={() => {}} onCancel={() => {}} running={false}/>
    );
    const textarea = container.querySelector('textarea');

    expect(textarea.className).toContain('composer-input');
    expect(textarea.style.getPropertyValue('font-size')).toBe('17px');
    expect(textarea.style.getPropertyPriority('font-size')).toBe('important');
    expect(textarea.style.getPropertyValue('-webkit-text-size-adjust')).toBe('100%');

    textarea.style.removeProperty('font-size');
    fireEvent.touchStart(textarea);

    expect(textarea.style.getPropertyValue('font-size')).toBe('17px');
    expect(textarea.style.getPropertyPriority('font-size')).toBe('important');
  });

  it('focus guard can be applied idempotently to a textarea', () => {
    mockMobile(false);
    const textarea = document.createElement('textarea');

    pinComposerFocusGuard(textarea);
    pinComposerFocusGuard(textarea);

    expect(textarea.style.getPropertyValue('font-size')).toBe('16px');
    expect(textarea.style.getPropertyPriority('font-size')).toBe('important');
    expect(textarea.style.getPropertyValue('touch-action')).toBe('manipulation');
  });

  it('temporarily locks maximum scale while the composer is focused, then restores the page viewport', () => {
    vi.useFakeTimers();
    mockMobile(true);
    mockViewportScrollReset();
    const meta = document.createElement('meta');
    meta.name = 'viewport';
    meta.content = 'width=device-width, initial-scale=1';
    meta.dataset.testComposer = 'true';
    document.head.appendChild(meta);

    const { container } = render(
      <Composer onSend={() => {}} onCancel={() => {}} running={false}/>
    );
    const textarea = container.querySelector('textarea');

    fireEvent.touchStart(textarea);

    expect(meta.getAttribute('content')).toBe('width=device-width, initial-scale=1, maximum-scale=1');

    fireEvent.blur(textarea);
    vi.advanceTimersByTime(249);
    expect(meta.getAttribute('content')).toContain('maximum-scale=1');

    vi.advanceTimersByTime(1);
    expect(meta.getAttribute('content')).toBe('width=device-width, initial-scale=1');
  });

  it('normalizes the focus-time viewport lock without making zoom permanently inaccessible', () => {
    expect(composerLockedViewportContent('width=device-width, initial-scale=1, maximum-scale=4, user-scalable=yes'))
      .toBe('width=device-width, initial-scale=1, maximum-scale=1');
  });

  it('recenters the layout viewport after mobile composer focus settles', () => {
    mockMobile(true);
    const textarea = document.createElement('textarea');
    mockViewportScrollReset();
    document.documentElement.scrollTop = 123;
    document.body.scrollTop = 456;

    prepareComposerFocus(textarea);

    expect(window.requestAnimationFrame).toHaveBeenCalledTimes(2);
    expect(window.scrollTo).toHaveBeenCalledWith(0, 0);
    expect(document.documentElement.scrollTop).toBe(0);
    expect(document.body.scrollTop).toBe(0);
  });

  it('extracts only image files from clipboard data and dedupes item/file mirrors', () => {
    const image = new File(['png'], 'image.png', { type: 'image/png', lastModified: 1 });
    const text = new File(['hello'], 'note.txt', { type: 'text/plain', lastModified: 1 });
    const out = clipboardImageFiles({
      items: [
        { kind: 'file', type: 'text/plain', getAsFile: () => text },
        { kind: 'file', type: 'image/png', getAsFile: () => image },
      ],
      files: [image, text],
    });

    expect(out).toHaveLength(1);
    expect(out[0].name).toBe('pasted-image-1.png');
    expect(out[0].type).toBe('image/png');
  });

  it('adds pasted images as attachments and sends them with the turn', () => {
    const image = new File(['jpg'], 'image.jpeg', { type: 'image/jpeg', lastModified: 2 });
    const onSend = vi.fn();
    const { container, getByRole, getByText } = render(
      <Composer onSend={onSend} onCancel={() => {}} running={false}/>
    );
    const textarea = container.querySelector('textarea');

    const event = createEvent.paste(textarea, {
      clipboardData: {
        items: [{ kind: 'file', type: 'image/jpeg', getAsFile: () => image }],
        files: [],
      },
    });
    fireEvent(textarea, event);

    expect(event.defaultPrevented).toBe(true);
    expect(getByText('pasted-image-1.jpg')).toBeTruthy();

    fireEvent.click(getByRole('button', { name: /send/i }));

    expect(onSend).toHaveBeenCalledTimes(1);
    expect(onSend.mock.calls[0][0]).toBe('');
    expect(onSend.mock.calls[0][1]).toHaveLength(1);
    expect(onSend.mock.calls[0][1][0].name).toBe('pasted-image-1.jpg');
    expect(onSend.mock.calls[0][1][0].type).toBe('image/jpeg');
  });

  it('uses controlled per-conversation draft text and clears it after send', () => {
    const onDraftChange = vi.fn();
    const onSend = vi.fn();
    const { container, getByRole } = render(
      <Composer
        onSend={onSend}
        onCancel={() => {}}
        running={false}
        draftText="saved draft"
        draftAttachments={[]}
        onDraftChange={onDraftChange}/>
    );
    expect(container.querySelector('textarea').value).toBe('saved draft');
    fireEvent.click(getByRole('button', { name: /send/i }));
    expect(onSend).toHaveBeenCalledWith('saved draft', []);
    expect(onDraftChange).toHaveBeenLastCalledWith({ text: '', attachments: [] });
  });

  it('shows queued controls while running and toggles next-tool mode', () => {
    const onQueueModeChange = vi.fn();
    const { getByRole, getByText } = render(
      <Composer
        onSend={() => {}}
        onCancel={() => {}}
        running={true}
        queueMode="normal"
        nextRunModel={{ provider: 'p', model: 'next-model' }}
        onQueueModeChange={onQueueModeChange}/>
    );
    fireEvent.click(getByRole('button', { name: /next tool/i }));
    expect(onQueueModeChange).toHaveBeenCalledWith('next_tool_call');
    // While running, the send button switches to "Queue message".
    expect(getByRole('button', { name: 'Queue message' })).toBeTruthy();
    expect(getByText('next next-model')).toBeTruthy();
  });
});
