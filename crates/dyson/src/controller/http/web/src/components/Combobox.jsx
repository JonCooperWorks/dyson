import React from 'react';

/**
 * Reusable searchable single-select dropdown.
 *
 * Unlike a native <input list=datalist> (whose popup filters by the
 * committed label and so hides every other entry once one is chosen) this
 * renders its own list: type to filter, but the full list is always one
 * keystroke (clear the box) away, so no option can become unreachable.
 *
 *   options  : [{ value: string, label: string, hint?: string }]
 *   value    : currently-selected value ('' = none)
 *   onSelect : (option) => void   — fired when an option is chosen
 *   onClear  : () => void         — fired when the box is emptied (optional)
 *
 * Escape closes the list and stops propagating, so a combobox inside a
 * modal closes its own dropdown first rather than dismissing the modal.
 */
export function Combobox({
  options,
  value = '',
  onSelect,
  onClear,
  placeholder = '',
  disabled = false,
  ariaLabel,
}) {
  const opts = options || [];
  const selected = opts.find(o => o.value === value) || null;
  const committedLabel = selected ? selected.label : '';

  const [query, setQuery] = React.useState(committedLabel);
  const [open, setOpen] = React.useState(false);
  const [activeIndex, setActiveIndex] = React.useState(-1);
  const listId = React.useId();

  React.useEffect(() => { setQuery(committedLabel); }, [committedLabel]);

  // While the box still shows the committed label the user hasn't started a
  // new search, so show everything; once they edit, filter by the text.
  const editing = query.trim() !== committedLabel.trim();
  const needle = editing ? query.trim().toLowerCase() : '';
  const visible = needle
    ? opts.filter(o =>
      o.label.toLowerCase().includes(needle) ||
      String(o.value).toLowerCase().includes(needle))
    : opts;

  const exactMatch = (text) => {
    const t = text.trim().toLowerCase();
    if (!t) return null;
    return opts.find(o =>
      o.label.toLowerCase() === t || String(o.value).toLowerCase() === t) || null;
  };

  const commit = (opt) => {
    if (!opt) return;
    onSelect?.(opt);
    setQuery(opt.label);
    setOpen(false);
    setActiveIndex(-1);
  };

  const onChange = (e) => {
    setQuery(e.target.value);
    setOpen(true);
    setActiveIndex(-1);
    if (!e.target.value.trim()) onClear?.();
  };

  const onKeyDown = (e) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setOpen(true);
      setActiveIndex(i => Math.min(i + 1, visible.length - 1));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActiveIndex(i => Math.max(i - 1, 0));
    } else if (e.key === 'Enter') {
      if (!open) return;
      e.preventDefault();
      const pick = (activeIndex >= 0 && activeIndex < visible.length)
        ? visible[activeIndex]
        : (exactMatch(query) || (visible.length === 1 ? visible[0] : null));
      if (pick) commit(pick);
    } else if (e.key === 'Escape') {
      if (!open) return;
      // Close our own list instead of letting a parent modal handle Esc.
      e.preventDefault();
      e.stopPropagation();
      setOpen(false);
      setQuery(committedLabel);
      setActiveIndex(-1);
    }
  };

  return (
    <div className="combobox">
      <input
        type="text"
        role="combobox"
        aria-expanded={open}
        aria-controls={listId}
        aria-autocomplete="list"
        autoComplete="off"
        autoCorrect="off"
        autoCapitalize="off"
        spellCheck={false}
        aria-label={ariaLabel}
        className="combobox-input"
        value={query}
        placeholder={placeholder}
        disabled={disabled}
        onChange={onChange}
        onFocus={(e) => { setOpen(true); e.target.select(); }}
        onBlur={() => { setOpen(false); setQuery(committedLabel); setActiveIndex(-1); }}
        onKeyDown={onKeyDown}
      />
      {open && !disabled ? (
        <ul className="combobox-list" id={listId} role="listbox">
          {visible.length === 0 ? (
            <li className="combobox-empty">no matches</li>
          ) : visible.map((opt, i) => (
            <li
              key={opt.value}
              role="option"
              aria-selected={opt.value === value}
              className={`combobox-option${i === activeIndex ? ' is-active' : ''}`}
              // mousedown (not click) so it fires before the input's blur
              onMouseDown={(e) => { e.preventDefault(); commit(opt); }}
            >
              <span className="combobox-option-label">{opt.label}</span>
              {opt.hint ? <span className="combobox-hint">{opt.hint}</span> : null}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}
