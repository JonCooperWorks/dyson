import React from 'react';
import { describe, it, expect, afterEach, vi } from 'vitest';
import { render, fireEvent, createEvent, cleanup } from '@testing-library/react';
import { Composer, clipboardImageFiles } from '../components/turns.jsx';

afterEach(() => cleanup());

describe('Composer image paste', () => {
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
});
