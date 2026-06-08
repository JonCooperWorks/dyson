import React from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';

import { ElicitationModal } from '../components/ElicitationModal.jsx';
import { ApiProvider } from '../hooks/useApi.js';

afterEach(() => {
  cleanup();
});

function mockClient({ pending }) {
  return {
    listElicitations: vi.fn(async () => ({ pending })),
    respondElicitation: vi.fn(async () => ({ ok: true })),
  };
}

function renderWith(client) {
  return render(
    <ApiProvider client={client}>
      <ElicitationModal />
    </ApiProvider>,
  );
}

describe('ElicitationModal', () => {
  it('renders nothing when there are no open prompts', async () => {
    const client = mockClient({ pending: [] });
    renderWith(client);
    await waitFor(() => expect(client.listElicitations).toHaveBeenCalled());
    expect(screen.queryByRole('dialog')).toBeNull();
  });

  it('shows a prompt, collects schema input, and submits an accept', async () => {
    const client = mockClient({
      pending: [
        {
          id: '7',
          server: 'everything',
          message: 'What is your name?',
          requestedSchema: {
            type: 'object',
            properties: { name: { type: 'string', title: 'name' } },
            required: ['name'],
          },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('What is your name?'));

    fireEvent.change(screen.getByLabelText(/name/), { target: { value: 'Ada' } });
    fireEvent.click(screen.getByText('Submit'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
    expect(client.respondElicitation).toHaveBeenCalledWith('7', {
      action: 'accept',
      content: { name: 'Ada' },
    });
  });

  it('submits a decline without content', async () => {
    const client = mockClient({
      pending: [
        {
          id: '9',
          server: 'everything',
          message: 'Allow?',
          requestedSchema: { type: 'object' },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Allow?'));
    fireEvent.click(screen.getByText('Decline'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
    expect(client.respondElicitation).toHaveBeenCalledWith('9', { action: 'decline' });
  });

  it('shows the server name in the header', async () => {
    const client = mockClient({
      pending: [
        {
          id: '1',
          server: 'sparky-tools',
          message: 'Confirm?',
          requestedSchema: { type: 'object' },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Confirm?'));
    expect(screen.getByText(/sparky-tools/)).toBeTruthy();
  });

  it('coerces numbers and omits empty optional strings', async () => {
    const client = mockClient({
      pending: [
        {
          id: '3',
          server: 'srv',
          message: 'Cfg',
          requestedSchema: {
            type: 'object',
            properties: {
              limit: { type: 'integer', title: 'Limit' },
              note: { type: 'string', title: 'Note' },
            },
            required: ['limit'],
          },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Cfg'));
    fireEvent.change(screen.getByLabelText(/Limit/), { target: { value: '42' } });
    fireEvent.click(screen.getByText('Submit'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
    // Empty optional `note` is omitted entirely (not sent as "").
    expect(client.respondElicitation).toHaveBeenCalledWith('3', {
      action: 'accept',
      content: { limit: 42 },
    });
  });

  it('renders enum as a select with a placeholder', async () => {
    const client = mockClient({
      pending: [
        {
          id: '4',
          server: 'srv',
          message: 'Pick one',
          requestedSchema: {
            type: 'object',
            properties: {
              colour: { type: 'string', enum: ['red', 'green', 'blue'], title: 'Colour' },
            },
            required: ['colour'],
          },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Pick one'));
    fireEvent.change(screen.getByLabelText(/Colour/), { target: { value: 'green' } });
    fireEvent.click(screen.getByText('Submit'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
    expect(client.respondElicitation).toHaveBeenCalledWith('4', {
      action: 'accept',
      content: { colour: 'green' },
    });
  });

  it('blocks submit when a required field is empty and surfaces "required"', async () => {
    const client = mockClient({
      pending: [
        {
          id: '5',
          server: 'srv',
          message: 'Need value',
          requestedSchema: {
            type: 'object',
            properties: { who: { type: 'string', title: 'Who' } },
            required: ['who'],
          },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Need value'));
    fireEvent.click(screen.getByText('Submit'));

    // No call went out — required validation gated it.
    expect(client.respondElicitation).not.toHaveBeenCalled();
    // Now fill it in and resubmit.
    fireEvent.change(screen.getByLabelText(/Who/), { target: { value: 'me' } });
    fireEvent.click(screen.getByText('Submit'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
  });

  it('Escape cancels the prompt', async () => {
    const client = mockClient({
      pending: [
        {
          id: '6',
          server: 'srv',
          message: 'Hit ESC',
          requestedSchema: { type: 'object' },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Hit ESC'));
    // Re-dispatch Escape on each poll rather than once: the keydown listener
    // attaches in an effect a tick after the prompt text renders, and under
    // full-suite load the async cancel can resolve just past the default
    // 1s waitFor budget (observed 1028ms). Both made this flaky in CI/build;
    // retrying the dispatch with a wider window is deterministic.
    await waitFor(
      () => {
        window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
        expect(client.respondElicitation).toHaveBeenCalled();
      },
      { timeout: 3000 },
    );
    expect(client.respondElicitation).toHaveBeenCalledWith('6', { action: 'cancel' });
  });

  it('renders field descriptions as help text', async () => {
    const client = mockClient({
      pending: [
        {
          id: '7',
          server: 'srv',
          message: 'Detail',
          requestedSchema: {
            type: 'object',
            properties: {
              email: {
                type: 'string',
                format: 'email',
                title: 'Email',
                description: "We'll only use this to confirm the action.",
              },
            },
          },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Detail'));
    expect(screen.getByText(/only use this to confirm/i)).toBeTruthy();
  });
});
