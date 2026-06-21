/* Tests for the shared <Modal> base — the systematic Esc / scrim dismiss. */
import { describe, expect, test, afterEach, vi } from 'vitest';
import React from 'react';
import { render, screen, fireEvent, cleanup } from '@testing-library/react';
import '@testing-library/jest-dom/vitest';

import { Modal } from './Modal.jsx';

afterEach(cleanup);

function renderModal(props = {}) {
  const onClose = props.onClose || vi.fn();
  render(
    React.createElement(
      Modal,
      { onClose, label: 'test dialog', ...props },
      React.createElement('button', { key: 'b' }, 'inside'),
    ),
  );
  return onClose;
}

describe('Modal', () => {
  test('renders children inside a dialog box within the scrim', () => {
    renderModal();
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveClass('modal');
    expect(dialog).toHaveAttribute('aria-modal', 'true');
    expect(dialog).toHaveAttribute('aria-label', 'test dialog');
    expect(screen.getByText('inside')).toBeInTheDocument();
  });

  test('Escape dismisses', () => {
    const onClose = renderModal();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  test('clicking the scrim dismisses, clicking the dialog does not', () => {
    const onClose = renderModal();
    fireEvent.click(screen.getByText('inside')); // inside the dialog
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.click(document.querySelector('.modal-scrim')); // the backdrop itself
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  test('closeOnScrimClick={false} keeps the scrim inert but Escape still works', () => {
    const onClose = renderModal({ closeOnScrimClick: false });
    fireEvent.click(document.querySelector('.modal-scrim'));
    expect(onClose).not.toHaveBeenCalled();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  test('className / scrimClassName override the defaults', () => {
    render(
      React.createElement(
        Modal,
        { onClose: vi.fn(), label: 'x', className: 'cmdpal', scrimClassName: 'cmdpal-scrim' },
        'body',
      ),
    );
    const dialog = screen.getByRole('dialog');
    expect(dialog).toHaveClass('cmdpal');
    expect(dialog).not.toHaveClass('modal');
    expect(document.querySelector('.cmdpal-scrim')).toBeTruthy();
    expect(document.querySelector('.modal-scrim')).toBeNull();
  });

  test('removes its key listener on unmount', () => {
    const onClose = vi.fn();
    const { unmount } = render(
      React.createElement(Modal, { onClose, label: 'x' }, 'body'),
    );
    unmount();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).not.toHaveBeenCalled();
  });
});
