// composer-checkpoint-upload.test.jsx
//
// Pins the SecurityCheckpoint upload flow on the Composer.  When the
// operator attaches a JSON file whose shape matches a security_engineer
// harness checkpoint, the Composer should:
//   1. POST the file body to /api/mind/file at
//      kb/security-harness/checkpoints/<run_id>.json
//   2. NOT enqueue the file in the attachment list (no inline base64
//      send via run_with_attachments)
//   3. Drop a "Please resume security_engineer from <run_id>" line into
//      the draft so the next send fires the harness with resume=true
//   4. Surface a transient toast naming the path
//
// Non-checkpoint JSON files and non-JSON files should fall through
// unchanged to the existing attachment flow.

import React from 'react';
import { describe, it, expect, vi } from 'vitest';
import { render, screen, waitFor, fireEvent } from '@testing-library/react';
import { Composer } from '../components/turns.jsx';
import { ApiProvider } from '../hooks/useApi.js';

function mountComposer({ onSend, mockClient } = {}) {
  let draft = { text: '', attachments: [] };
  const handleDraftChange = next => { draft = next; };
  const utils = render(
    <ApiProvider client={mockClient || {}}>
      <Composer
        onSend={onSend || (() => {})}
        running={false}
        draftText=""
        draftAttachments={[]}
        onDraftChange={handleDraftChange}/>
    </ApiProvider>
  );
  return { ...utils, getDraft: () => draft };
}

// Constructs a checkpoint payload that matches parseSecurityCheckpoint's
// triple-key signature.  All three fields must be present and well-formed.
function checkpointPayload(overrides = {}) {
  return JSON.stringify({
    schema_version: 1,
    harness_version: 'security-harness-v1',
    run_id: 'sec-1780830172-2',
    current_stage: 'validate',
    findings_so_far: [],
    ...overrides,
  });
}

function fileFromText(name, text, type = 'application/json') {
  return new File([text], name, { type });
}

describe('Composer — security_engineer checkpoint upload', () => {
  it('detects a checkpoint .json and POSTs to /api/mind/file at the canonical path', async () => {
    const postMindFile = vi.fn(() => Promise.resolve());
    const { container, getDraft } = mountComposer({ mockClient: { postMindFile } });
    const fileInput = container.querySelector('input[type="file"]');

    const cp = checkpointPayload({ run_id: 'sec-abc-1' });
    fireEvent.change(fileInput, { target: { files: [fileFromText('cp.json', cp)] } });

    await waitFor(() => expect(postMindFile).toHaveBeenCalledTimes(1));
    expect(postMindFile).toHaveBeenCalledWith(
      'kb/security-harness/checkpoints/sec-abc-1.json',
      cp,
    );
    // The draft should now contain a resume prompt naming the run_id.
    await waitFor(() => {
      expect(getDraft().text).toMatch(/resume.*sec-abc-1/i);
    });
    // The toast surface should also name the run_id.
    expect(container.querySelector('[data-testid="checkpoint-toast"]')?.textContent)
      .toMatch(/sec-abc-1/);
    // And the file should NOT be in the attachments list (its purpose
    // was to seed a resume, not to be sent inline).
    expect(getDraft().attachments).toEqual([]);
  });

  it('preserves existing draft text when prepending the resume prompt', async () => {
    const postMindFile = vi.fn(() => Promise.resolve());
    let draft = { text: 'pre-existing thought', attachments: [] };
    const onDraftChange = next => { draft = next; };
    const { container } = render(
      <ApiProvider client={{ postMindFile }}>
        <Composer
          onSend={() => {}}
          running={false}
          draftText="pre-existing thought"
          draftAttachments={[]}
          onDraftChange={onDraftChange}/>
      </ApiProvider>
    );
    const fileInput = container.querySelector('input[type="file"]');
    fireEvent.change(fileInput, {
      target: { files: [fileFromText('c.json', checkpointPayload())] },
    });
    await waitFor(() => expect(postMindFile).toHaveBeenCalled());
    expect(draft.text.startsWith('pre-existing thought')).toBe(true);
    expect(draft.text).toMatch(/resume.*sec-1780830172-2/i);
  });

  it('ignores a .json file that lacks the checkpoint signature (no schema_version)', async () => {
    const postMindFile = vi.fn(() => Promise.resolve());
    const { container, getDraft } = mountComposer({ mockClient: { postMindFile } });
    const fileInput = container.querySelector('input[type="file"]');
    const notCp = JSON.stringify({ hello: 'world', schema_version: 'not-a-number' });
    fireEvent.change(fileInput, {
      target: { files: [fileFromText('config.json', notCp)] },
    });
    // Wait a tick for any async processing.
    await new Promise(r => setTimeout(r, 50));
    expect(postMindFile).not.toHaveBeenCalled();
    // Should have been routed to attachments instead.
    expect(getDraft().attachments.length).toBe(1);
  });

  it('ignores non-.json files outright', async () => {
    const postMindFile = vi.fn(() => Promise.resolve());
    const { container, getDraft } = mountComposer({ mockClient: { postMindFile } });
    const fileInput = container.querySelector('input[type="file"]');
    fireEvent.change(fileInput, {
      target: { files: [new File(['hello'], 'note.md', { type: 'text/markdown' })] },
    });
    await new Promise(r => setTimeout(r, 50));
    expect(postMindFile).not.toHaveBeenCalled();
    expect(getDraft().attachments.length).toBe(1);
  });

  it('shows an error toast when the POST fails (network error / 4xx)', async () => {
    const postMindFile = vi.fn(() => Promise.reject(new Error('save failed: 500')));
    const { container } = mountComposer({ mockClient: { postMindFile } });
    const fileInput = container.querySelector('input[type="file"]');
    fireEvent.change(fileInput, {
      target: { files: [fileFromText('c.json', checkpointPayload())] },
    });
    await waitFor(() => {
      const toast = container.querySelector('[data-testid="checkpoint-toast"]');
      expect(toast?.textContent || '').toMatch(/failed.*500/);
    });
  });

  it('falls through cleanly when no ApiProvider is mounted (degrades gracefully)', async () => {
    // Bare Composer mount, no ApiProvider — the checkpoint upload affordance
    // is "nice to have" and should not break the composer's core behavior
    // (text entry, attachment fallback for the same file).
    let draft = { text: '', attachments: [] };
    const { container } = render(
      <Composer
        onSend={() => {}}
        running={false}
        draftText=""
        draftAttachments={[]}
        onDraftChange={next => { draft = next; }}/>
    );
    const fileInput = container.querySelector('input[type="file"]');
    fireEvent.change(fileInput, {
      target: { files: [fileFromText('c.json', checkpointPayload())] },
    });
    await new Promise(r => setTimeout(r, 50));
    // Without an API client, the file becomes an attachment instead.
    expect(draft.attachments.length).toBe(1);
    expect(draft.text).toBe('');
  });

  it('routes mixed picks — one checkpoint goes to mind, other files stay as attachments', async () => {
    const postMindFile = vi.fn(() => Promise.resolve());
    const { container, getDraft } = mountComposer({ mockClient: { postMindFile } });
    const fileInput = container.querySelector('input[type="file"]');
    fireEvent.change(fileInput, {
      target: {
        files: [
          fileFromText('checkpoint.json', checkpointPayload({ run_id: 'sec-mix' })),
          new File(['png-bytes'], 'shot.png', { type: 'image/png' }),
        ],
      },
    });
    await waitFor(() => expect(postMindFile).toHaveBeenCalledTimes(1));
    expect(postMindFile).toHaveBeenCalledWith(
      'kb/security-harness/checkpoints/sec-mix.json',
      expect.any(String),
    );
    // Only the PNG should remain as an attachment.
    const atts = getDraft().attachments;
    expect(atts.length).toBe(1);
    expect(atts[0].name).toBe('shot.png');
  });
});
