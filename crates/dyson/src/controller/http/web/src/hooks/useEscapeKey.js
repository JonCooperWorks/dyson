import React from 'react';

/**
 * Invoke `handler` whenever Escape is pressed while the component is
 * mounted.  Passing a falsy handler disables the listener.  Used by the
 * shared <Modal> so every modal is Escape-dismissable the same way.
 */
export function useEscapeKey(handler) {
  React.useEffect(() => {
    if (!handler) return undefined;
    const onKey = (e) => {
      if (e.key === 'Escape') handler(e);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [handler]);
}
