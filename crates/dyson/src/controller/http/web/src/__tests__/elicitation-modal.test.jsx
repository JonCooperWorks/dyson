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
          message: 'What is your name?',
          requestedSchema: {
            type: 'object',
            properties: { name: { type: 'string', title: 'name' } },
          },
        },
      ],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('What is your name?'));

    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Ada' } });
    fireEvent.click(screen.getByText('Submit'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
    expect(client.respondElicitation).toHaveBeenCalledWith('7', {
      action: 'accept',
      content: { name: 'Ada' },
    });
  });

  it('submits a decline without content', async () => {
    const client = mockClient({
      pending: [{ id: '9', message: 'Allow?', requestedSchema: { type: 'object' } }],
    });
    renderWith(client);

    await waitFor(() => screen.getByText('Allow?'));
    fireEvent.click(screen.getByText('Decline'));

    await waitFor(() => expect(client.respondElicitation).toHaveBeenCalled());
    expect(client.respondElicitation).toHaveBeenCalledWith('9', { action: 'decline' });
  });
});
