/* Dyson — MCP elicitation modal.
 *
 * A connected MCP server can ask the user to fill in a small form mid
 * tool-call (`elicitation/create`).  The backend parks the request in a
 * process-global broker; this component short-polls `GET
 * /api/mcp/elicitations`, renders the first open prompt as a form built
 * from its `requestedSchema`, and submits the answer to
 * `POST /api/mcp/elicitations/:id` as the MCP `ElicitResult`
 * (`{ action: "accept"|"decline"|"cancel", content? }`).
 *
 * Polling (vs. a dedicated SSE channel) keeps this off the per-chat SSE
 * hot path and needs no ticket-auth dance — elicitation is not latency
 * critical.
 */

import React, { useState, useEffect, useCallback } from 'react';
import { useApi } from '../hooks/useApi.js';

const POLL_MS = 2000;

/** Build the initial form state from a JSON-schema `properties` object. */
function initialValues(schema) {
  const props = (schema && schema.properties) || {};
  const out = {};
  for (const [key, spec] of Object.entries(props)) {
    out[key] = spec && spec.type === 'boolean' ? false : '';
  }
  return out;
}

/** One field per schema property.  Strings/numbers → text input,
 *  booleans → checkbox, enums → select.  Unknown shapes fall back to a
 *  text input so the user can always answer. */
function SchemaField({ name, spec, value, onChange }) {
  const label = (spec && spec.title) || name;
  const required = spec && spec.required === true;
  if (spec && Array.isArray(spec.enum)) {
    return (
      <label className="elicit-field">
        <span>{label}{required ? ' *' : ''}</span>
        <select value={value} onChange={(e) => onChange(name, e.target.value)}>
          <option value="" />
          {spec.enum.map((opt) => (
            <option key={String(opt)} value={String(opt)}>{String(opt)}</option>
          ))}
        </select>
      </label>
    );
  }
  if (spec && spec.type === 'boolean') {
    return (
      <label className="elicit-field elicit-bool">
        <input
          type="checkbox"
          checked={!!value}
          onChange={(e) => onChange(name, e.target.checked)}
        />
        <span>{label}</span>
      </label>
    );
  }
  const inputType = spec && (spec.type === 'number' || spec.type === 'integer') ? 'number' : 'text';
  return (
    <label className="elicit-field">
      <span>{label}{required ? ' *' : ''}</span>
      <input
        type={inputType}
        value={value}
        onChange={(e) => onChange(name, e.target.value)}
      />
    </label>
  );
}

export function ElicitationModal() {
  const api = useApi();
  const [prompt, setPrompt] = useState(null); // the first open prompt, or null
  const [values, setValues] = useState({});
  const [busy, setBusy] = useState(false);

  const poll = useCallback(async () => {
    // Don't clobber a prompt the user is mid-answer on.
    if (busy) return;
    try {
      const res = await api.listElicitations();
      const next = (res && res.pending && res.pending[0]) || null;
      setPrompt((cur) => {
        // Only (re)seed the form when the prompt identity changes.
        if (next && (!cur || cur.id !== next.id)) {
          setValues(initialValues(next.requestedSchema));
        }
        return next;
      });
    } catch {
      // Network blips are transient; the next tick retries.
    }
  }, [api, busy]);

  useEffect(() => {
    poll();
    const t = setInterval(poll, POLL_MS);
    return () => clearInterval(t);
  }, [poll]);

  if (!prompt) return null;

  const onChange = (name, v) => setValues((cur) => ({ ...cur, [name]: v }));

  const submit = async (action) => {
    setBusy(true);
    try {
      const result = action === 'accept' ? { action, content: values } : { action };
      await api.respondElicitation(prompt.id, result);
      setPrompt(null);
      setValues({});
    } catch {
      // Leave the modal open so the user can retry.
    } finally {
      setBusy(false);
    }
  };

  const props = (prompt.requestedSchema && prompt.requestedSchema.properties) || {};
  const fields = Object.entries(props);

  return (
    <div className="elicit-overlay" role="dialog" aria-modal="true" aria-label="MCP request">
      <div className="elicit-modal">
        <div className="elicit-message">{prompt.message || 'The MCP server is requesting input.'}</div>
        <div className="elicit-fields">
          {fields.map(([name, spec]) => (
            <SchemaField
              key={name}
              name={name}
              spec={spec}
              value={values[name] ?? ''}
              onChange={onChange}
            />
          ))}
        </div>
        <div className="elicit-actions">
          <button type="button" disabled={busy} onClick={() => submit('accept')}>Submit</button>
          <button type="button" disabled={busy} onClick={() => submit('decline')}>Decline</button>
          <button type="button" disabled={busy} onClick={() => submit('cancel')}>Cancel</button>
        </div>
      </div>
    </div>
  );
}
