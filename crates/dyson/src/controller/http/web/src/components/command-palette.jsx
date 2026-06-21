import React, { useEffect, useMemo, useRef, useState } from 'react';
import { Icon, Kbd } from './icons.jsx';
import { NAVS } from './views.jsx';
import { FALLBACK_SLASH_COMMANDS } from '../store/constants.js';
import { Modal } from 'dyson-common-ui';

export function buildCommandItems({
  conversations = [],
  providers = [],
  mindFiles = [],
  onSelectConversation,
  onSelectView,
  onSelectModel,
  onOpenMindFile,
  onInsertSlash,
  slashCommands = FALLBACK_SLASH_COMMANDS,
} = {}) {
  const items = [];
  const commands = Array.isArray(slashCommands) && slashCommands.length
    ? slashCommands
    : FALLBACK_SLASH_COMMANDS;
  for (const nav of NAVS) {
    items.push({
      id: `view:${nav.id}`,
      kind: 'View',
      label: nav.name,
      hint: `Switch to ${nav.name}`,
      icon: nav.icon,
      run: () => onSelectView?.(nav.id),
    });
  }
  for (const c of conversations) {
    items.push({
      id: `conversation:${c.id}`,
      kind: 'Conversation',
      label: c.title || c.id,
      hint: c.id,
      icon: 'chat',
      run: () => onSelectConversation?.(c.id),
    });
  }
  for (const p of providers) {
    for (const model of (p.models || [])) {
      items.push({
        id: `model:${p.id}:${model}`,
        kind: 'Model',
        label: model,
        hint: p.name || p.id,
        icon: 'dot',
        run: () => onSelectModel?.(p.id, model),
      });
    }
  }
  for (const f of mindFiles) {
    items.push({
      id: `mind:${f.path}`,
      kind: 'Mind',
      label: f.path,
      hint: f.size || 'workspace file',
      icon: 'file',
      run: () => onOpenMindFile?.(f.path),
    });
  }
  for (const c of commands) {
    items.push({
      id: `slash:${c.cmd}`,
      kind: 'Command',
      label: c.cmd,
      hint: c.desc,
      icon: 'slash',
      run: () => onInsertSlash?.(c.cmd),
    });
  }
  return items;
}

function matches(item, query) {
  if (!query) return true;
  const q = query.toLowerCase();
  return [item.kind, item.label, item.hint]
    .filter(Boolean)
    .some(v => String(v).toLowerCase().includes(q));
}

export function CommandPalette({ open, onClose, ...sources }) {
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const inputRef = useRef(null);
  const items = useMemo(() => buildCommandItems(sources), [sources]);
  const visible = items.filter(item => matches(item, query)).slice(0, 80);
  const focused = visible[Math.min(active, Math.max(0, visible.length - 1))];

  useEffect(() => {
    if (!open) return;
    setQuery('');
    setActive(0);
    const id = setTimeout(() => inputRef.current?.focus(), 0);
    return () => clearTimeout(id);
  }, [open]);

  useEffect(() => {
    if (active >= visible.length) setActive(Math.max(0, visible.length - 1));
  }, [active, visible.length]);

  if (!open) return null;

  const pick = (item) => {
    if (!item) return;
    item.run?.();
    onClose?.();
  };

  const onKeyDown = (e) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setActive(i => Math.min(i + 1, Math.max(0, visible.length - 1)));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActive(i => Math.max(0, i - 1));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      pick(focused);
    }
  };

  return (
    <Modal scrimClassName="cmdpal-scrim" className="cmdpal" label="Command palette" onClose={onClose}>
        <div className="cmdpal-input">
          <Icon name="search" size={14}/>
          <input
            ref={inputRef}
            value={query}
            onChange={e => { setQuery(e.target.value); setActive(0); }}
            onKeyDown={onKeyDown}
            placeholder="Jump to..."
            aria-label="Command palette search"/>
          <Kbd>Esc</Kbd>
        </div>
        <div className="cmdpal-list" role="listbox">
          {visible.length === 0 ? (
            <div className="cmdpal-empty">No matches</div>
          ) : visible.map((item, i) => (
            <button
              key={item.id}
              className={`cmdpal-item ${i === active ? 'active' : ''}`}
              role="option"
              aria-selected={i === active ? 'true' : 'false'}
              onMouseEnter={() => setActive(i)}
              onClick={() => pick(item)}>
              <Icon name={item.icon} size={13}/>
              <span className="main">
                <span className="label">{item.label}</span>
                <span className="hint">{item.hint}</span>
              </span>
              <span className="kind">{item.kind}</span>
            </button>
          ))}
        </div>
    </Modal>
  );
}
