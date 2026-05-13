import React, { useState } from 'react';

// TTL picker for the share-mint affordance.  Inline dropdown rather
// than a modal: minting a share is a one-decision flow and the
// resulting URL gets surfaced near the action that created it.
export function ShareMenu({ canShare, busy, onMint }) {
  const [open, setOpen] = useState(false);
  const ref = React.useRef(null);
  React.useEffect(() => {
    if (!open) return;
    const onDoc = (e) => {
      if (ref.current && !ref.current.contains(e.target)) setOpen(false);
    };
    document.addEventListener('mousedown', onDoc);
    return () => document.removeEventListener('mousedown', onDoc);
  }, [open]);
  const pick = (ttl) => { setOpen(false); onMint(ttl); };
  return (
    <span ref={ref} style={{ position: 'relative', display: 'inline-block' }}>
      <button
        className="btn sm ghost"
        onClick={() => setOpen(o => !o)}
        disabled={!canShare || busy}
        title="anonymous shareable link"
      >
        {busy ? 'minting…' : 'share…'}
      </button>
      {open && (
        <div role="menu" style={{
          position: 'absolute', right: 0, top: '100%', marginTop: 4,
          background: 'var(--panel)', border: '1px solid var(--line)',
          borderRadius: 6, padding: 4, zIndex: 20, display: 'flex',
          flexDirection: 'column', minWidth: 110,
          boxShadow: '0 4px 12px rgba(0,0,0,0.35)',
        }}>
          <button className="btn xs ghost" onClick={() => pick('1d')}>1 day</button>
          <button className="btn xs ghost" onClick={() => pick('7d')}>7 days</button>
          <button className="btn xs ghost" onClick={() => pick('30d')}>30 days</button>
          <button className="btn xs ghost" onClick={() => pick('never')} title="revoke manually from the shared tab">never</button>
        </div>
      )}
    </span>
  );
}
