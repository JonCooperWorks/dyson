import React from 'react';
import { useEscapeKey } from '../hooks/useEscapeKey.js';

/**
 * Shared modal shell: a full-screen scrim with a centred dialog box.
 *
 * Owns the cross-cutting modal behaviour so every modal gets it the same
 * way instead of re-implementing it: Escape dismisses, clicking the scrim
 * (but not the dialog itself) dismisses, and the box carries the dialog
 * a11y roles.  Callers pass their header / body / actions as children and
 * keep their own markup.
 *
 * Pass `closeOnScrimClick={false}` for flows where an accidental
 * backdrop click should not discard work.  `className` / `scrimClassName`
 * override the default classes so a modal with its own look (e.g. a
 * command palette) can still reuse the shared dismiss behaviour.
 */
export function Modal({
  onClose,
  label,
  labelledBy,
  className = 'modal',
  scrimClassName = 'modal-scrim',
  closeOnScrimClick = true,
  children,
}) {
  useEscapeKey(onClose);
  const onScrimClick = (e) => {
    if (closeOnScrimClick && e.target === e.currentTarget) onClose?.();
  };
  return (
    <div className={scrimClassName} onClick={onScrimClick}>
      <div
        className={className}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        aria-labelledby={labelledBy}
      >
        {children}
      </div>
    </div>
  );
}
