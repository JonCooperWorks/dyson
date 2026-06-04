/* Dyson — MCP elicitation modal.
 *
 * A connected MCP server can ask the user to fill in a small form mid
 * tool-call (`elicitation/create`).  The backend parks the request in a
 * process-global broker; this component short-polls `GET
 * /api/mcp/elicitations`, renders the oldest open prompt as a form built
 * from its `requestedSchema`, and submits the answer to
 * `POST /api/mcp/elicitations/:id` as the MCP `ElicitResult`
 * (`{ action: "accept"|"decline"|"cancel", content? }`).
 *
 * Polling (vs. a dedicated SSE channel) keeps this off the per-chat SSE
 * hot path and needs no ticket-auth dance — elicitation is not latency
 * critical, but we poll fast while idle so the prompt lands quickly.
 *
 * The MCP spec restricts `requestedSchema` to a flat object of primitive
 * properties (string / number / integer / boolean, optional `enum`,
 * string `format` of email/uri/date/date-time, plus min/max bounds), so
 * we lean on that: build typed inputs, validate against the constraints
 * the spec admits, and refuse to submit when required fields are missing.
 */

import React, { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { useApi } from '../hooks/useApi.js';

/** Idle poll interval — fast enough that a server-side elicit lands
 *  promptly, slow enough not to spam the controller.  Once a prompt is on
 *  screen we stop polling entirely until the user dismisses it. */
const IDLE_POLL_MS = 500;

/** Build the initial form state from a JSON-schema `properties` object.
 *  Honor `default` where the spec admits it; otherwise booleans start
 *  false and everything else starts empty (so required validation fires). */
function initialValues(schema) {
  const props = (schema && schema.properties) || {};
  const out = {};
  for (const [key, spec] of Object.entries(props)) {
    if (spec && Object.prototype.hasOwnProperty.call(spec, 'default')) {
      out[key] = spec.default;
    } else if (spec && spec.type === 'boolean') {
      out[key] = false;
    } else {
      out[key] = '';
    }
  }
  return out;
}

/** The set of required field names declared on the schema. */
function requiredSet(schema) {
  const arr = (schema && Array.isArray(schema.required)) ? schema.required : [];
  return new Set(arr);
}

/** Map a JSON-schema string `format` to an <input type>.  Anything we
 *  don't recognise falls back to "text" — the spec only blesses these. */
function htmlTypeForFormat(format) {
  switch (format) {
    case 'email': return 'email';
    case 'uri':   return 'url';
    case 'date':  return 'date';
    case 'date-time': return 'datetime-local';
    default: return 'text';
  }
}

/** Per-field validation against the subset of JSON-schema constraints
 *  the MCP elicitation spec admits.  Returns an error string or null. */
function validateField(spec, value, isRequired) {
  const empty = value === '' || value === null || value === undefined;
  if (isRequired && empty && !(spec && spec.type === 'boolean')) {
    return 'required';
  }
  if (empty) return null;
  if (!spec) return null;
  if (spec.type === 'number' || spec.type === 'integer') {
    const n = Number(value);
    if (Number.isNaN(n)) return 'must be a number';
    if (spec.type === 'integer' && !Number.isInteger(n)) return 'must be an integer';
    if (typeof spec.minimum === 'number' && n < spec.minimum) return `minimum ${spec.minimum}`;
    if (typeof spec.maximum === 'number' && n > spec.maximum) return `maximum ${spec.maximum}`;
  }
  if (spec.type === 'string') {
    const s = String(value);
    if (typeof spec.minLength === 'number' && s.length < spec.minLength) return `minimum ${spec.minLength} chars`;
    if (typeof spec.maxLength === 'number' && s.length > spec.maxLength) return `maximum ${spec.maxLength} chars`;
    if (spec.format === 'email' && !/^\S+@\S+\.\S+$/.test(s)) return 'must be an email';
    if (spec.format === 'uri' && !/^[a-z][a-z0-9+\-.]*:/i.test(s)) return 'must be a URI';
  }
  return null;
}

/** Coerce a form value to the JSON type the schema declares before we
 *  ship it back to the MCP server.  Empty optionals are omitted entirely
 *  so we never send `""` where the server expects a number. */
function coerceForWire(spec, value) {
  if (value === '' || value === null || value === undefined) return undefined;
  if (!spec) return value;
  if (spec.type === 'number' || spec.type === 'integer') {
    const n = Number(value);
    return Number.isNaN(n) ? undefined : n;
  }
  if (spec.type === 'boolean') return !!value;
  return value;
}

/** One field per schema property.  Type, enum, and format drive the
 *  input choice; description (if any) renders under the label as help
 *  text so the user knows what to type. */
function SchemaField({ name, spec, value, error, required, autoFocus, onChange }) {
  const label = (spec && spec.title) || name;
  const description = spec && typeof spec.description === 'string' ? spec.description : '';
  const showError = error && error !== 'required';

  const fieldId = `elicit-${name}`;
  const descId = description ? `${fieldId}-desc` : undefined;
  const errId = showError ? `${fieldId}-err` : undefined;
  const describedBy = [descId, errId].filter(Boolean).join(' ') || undefined;

  let control;
  if (spec && Array.isArray(spec.enum)) {
    const names = Array.isArray(spec.enumNames) ? spec.enumNames : null;
    control = (
      <select
        id={fieldId}
        value={value ?? ''}
        autoFocus={autoFocus}
        aria-describedby={describedBy}
        aria-invalid={!!error || undefined}
        onChange={(e) => onChange(name, e.target.value)}
      >
        <option value="">{required ? 'Select…' : '(none)'}</option>
        {spec.enum.map((opt, i) => (
          <option key={String(opt)} value={String(opt)}>{names ? names[i] ?? String(opt) : String(opt)}</option>
        ))}
      </select>
    );
  } else if (spec && spec.type === 'boolean') {
    return (
      <div className="elicit-field elicit-bool">
        <label htmlFor={fieldId}>
          <input
            id={fieldId}
            type="checkbox"
            checked={!!value}
            autoFocus={autoFocus}
            aria-describedby={describedBy}
            onChange={(e) => onChange(name, e.target.checked)}
          />
          <span className="elicit-label">{label}{required ? ' *' : ''}</span>
        </label>
        {description ? <div id={descId} className="elicit-desc">{description}</div> : null}
      </div>
    );
  } else {
    const inputType =
      spec && (spec.type === 'number' || spec.type === 'integer')
        ? 'number'
        : htmlTypeForFormat(spec && spec.format);
    const step = spec && spec.type === 'integer' ? '1' : undefined;
    control = (
      <input
        id={fieldId}
        type={inputType}
        value={value ?? ''}
        autoFocus={autoFocus}
        step={step}
        min={spec && typeof spec.minimum === 'number' ? spec.minimum : undefined}
        max={spec && typeof spec.maximum === 'number' ? spec.maximum : undefined}
        minLength={spec && typeof spec.minLength === 'number' ? spec.minLength : undefined}
        maxLength={spec && typeof spec.maxLength === 'number' ? spec.maxLength : undefined}
        aria-describedby={describedBy}
        aria-invalid={!!error || undefined}
        onChange={(e) => onChange(name, e.target.value)}
      />
    );
  }

  return (
    <div className={`elicit-field${error ? ' elicit-field-error' : ''}`}>
      <label htmlFor={fieldId} className="elicit-label">
        {label}{required ? ' *' : ''}
      </label>
      {description ? <div id={descId} className="elicit-desc">{description}</div> : null}
      {control}
      {showError ? <div id={errId} className="elicit-err">{error}</div> : null}
    </div>
  );
}

export function ElicitationModal() {
  const api = useApi();
  const [prompt, setPrompt] = useState(null); // the first open prompt, or null
  const [queueLength, setQueueLength] = useState(0); // total pending count
  const [values, setValues] = useState({});
  const [touched, setTouched] = useState({}); // per-field user interaction
  const [busy, setBusy] = useState(false);
  const formRef = useRef(null);

  const poll = useCallback(async () => {
    try {
      const res = await api.listElicitations();
      const pending = (res && res.pending) || [];
      const next = pending[0] || null;
      setQueueLength(pending.length);
      setPrompt((cur) => {
        // Only (re)seed the form when the prompt identity changes —
        // don't clobber the user mid-answer if poll fires concurrently.
        if (next && (!cur || cur.id !== next.id)) {
          setValues(initialValues(next.requestedSchema));
          setTouched({});
        }
        return next;
      });
    } catch {
      // Network blips are transient; the next tick retries.
    }
  }, [api]);

  // While idle (no prompt visible), poll fast; once a prompt is on screen
  // we stop polling — the user is busy filling it out and the next prompt
  // can wait until this one resolves.
  useEffect(() => {
    if (prompt) return undefined;
    poll();
    const t = setInterval(poll, IDLE_POLL_MS);
    return () => clearInterval(t);
  }, [poll, prompt]);

  const props = useMemo(() => {
    return (prompt && prompt.requestedSchema && prompt.requestedSchema.properties) || {};
  }, [prompt]);

  const required = useMemo(() => requiredSet(prompt && prompt.requestedSchema), [prompt]);

  const errors = useMemo(() => {
    const out = {};
    for (const [name, spec] of Object.entries(props)) {
      const err = validateField(spec, values[name], required.has(name));
      if (err) out[name] = err;
    }
    return out;
  }, [props, values, required]);

  const hasErrors = Object.keys(errors).length > 0;

  const onChange = (name, v) => {
    setValues((cur) => ({ ...cur, [name]: v }));
    setTouched((cur) => (cur[name] ? cur : { ...cur, [name]: true }));
  };

  const submit = useCallback(async (action) => {
    if (!prompt) return;
    if (action === 'accept') {
      // Block submit while validation is unsatisfied; mark every field
      // touched so the errors surface for the user.
      if (hasErrors) {
        const allTouched = {};
        for (const k of Object.keys(props)) allTouched[k] = true;
        setTouched(allTouched);
        return;
      }
    }
    setBusy(true);
    try {
      let result;
      if (action === 'accept') {
        const content = {};
        for (const [name, spec] of Object.entries(props)) {
          const v = coerceForWire(spec, values[name]);
          if (v !== undefined) content[name] = v;
        }
        result = { action, content };
      } else {
        result = { action };
      }
      await api.respondElicitation(prompt.id, result);
      setPrompt(null);
      setValues({});
      setTouched({});
    } catch {
      // Leave the modal open so the user can retry.
    } finally {
      setBusy(false);
    }
  }, [api, prompt, props, values, hasErrors]);

  // ESC cancels, Cmd/Ctrl-Enter accepts.  Enter alone is intentionally
  // *not* a submit shortcut: it would fire while the user is mid-typing
  // a number/string and skip validation surprises.
  useEffect(() => {
    if (!prompt) return undefined;
    const onKey = (e) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        submit('cancel');
      } else if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') {
        e.preventDefault();
        submit('accept');
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [prompt, submit]);

  if (!prompt) return null;

  const fields = Object.entries(props);
  const source = prompt.server ? `MCP · ${prompt.server}` : 'MCP request';

  return (
    <div className="elicit-overlay" role="dialog" aria-modal="true" aria-label="MCP request">
      <div className="elicit-modal">
        <div className="elicit-header">
          <span className="elicit-source">{source}</span>
          {queueLength > 1 ? (
            <span className="elicit-queue" title={`${queueLength} prompts waiting`}>
              1 of {queueLength}
            </span>
          ) : null}
        </div>
        <div className="elicit-message">
          {prompt.message || 'The MCP server is requesting input.'}
        </div>
        <form
          ref={formRef}
          className="elicit-fields"
          onSubmit={(e) => { e.preventDefault(); submit('accept'); }}
        >
          {fields.length === 0 ? (
            <div className="elicit-desc">No fields requested. Confirm to continue.</div>
          ) : null}
          {fields.map(([name, spec], i) => (
            <SchemaField
              key={name}
              name={name}
              spec={spec}
              value={values[name] ?? ''}
              error={touched[name] ? errors[name] : null}
              required={required.has(name)}
              autoFocus={i === 0}
              onChange={onChange}
            />
          ))}
          {/* Submit on Enter inside the form. */}
          <button type="submit" style={{ display: 'none' }} aria-hidden="true" tabIndex={-1} />
        </form>
        <div className="elicit-actions">
          <button
            type="button"
            className="elicit-cancel"
            disabled={busy}
            onClick={() => submit('cancel')}
          >
            Cancel
          </button>
          <button
            type="button"
            className="elicit-decline"
            disabled={busy}
            onClick={() => submit('decline')}
          >
            Decline
          </button>
          <button
            type="button"
            className="elicit-accept"
            disabled={busy || (fields.length > 0 && hasErrors && Object.keys(touched).length > 0)}
            onClick={() => submit('accept')}
          >
            Submit
          </button>
        </div>
        <div className="elicit-hint">Esc to cancel · ⌘/Ctrl + Enter to submit</div>
      </div>
    </div>
  );
}
