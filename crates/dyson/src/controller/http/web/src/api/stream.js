/* Dyson — SSE event parsing and dispatch.
 *
 * Pure functions: no EventSource, no network.  client.js owns the
 * EventSource and calls these on each message so the hot path stays
 * testable without mocking browser APIs.  Wire format matches what
 * controller/http/mod.rs emits for the /events stream.
 *
 * Event types:
 *   text        — streaming assistant prose (msg.delta)
 *   thinking    — streaming extended-thinking (msg.delta)
 *   tool_start  — { id, name }
 *   tool_result — { content, is_error, view? }
 *   checkpoint  — { text }
 *   file        — { name, mime_type, url, inline_image }
 *   artefact    — { id, kind, title, url, bytes, tool_use_id?, metadata? }
 *   llm_error   — { message }
 *   done        — terminator; caller should close the EventSource
 */

export function parseStreamEvent(raw) {
  if (typeof raw !== 'string') return null;
  let msg;
  try { msg = JSON.parse(raw); } catch { return null; }
  if (!msg || typeof msg !== 'object' || typeof msg.type !== 'string') return null;
  return msg;
}

export function dispatchStreamEvent(msg, callbacks) {
  if (!msg || !callbacks) return false;
  switch (msg.type) {
    case 'text':        callbacks.onText && callbacks.onText(msg.delta); return true;
    case 'thinking':    callbacks.onThinking && callbacks.onThinking(msg.delta); return true;
    case 'tool_start':  callbacks.onToolStart && callbacks.onToolStart(msg); return true;
    case 'tool_result': callbacks.onToolResult && callbacks.onToolResult(msg); return true;
    case 'checkpoint':  callbacks.onCheckpoint && callbacks.onCheckpoint(msg); return true;
    case 'file':        callbacks.onFile && callbacks.onFile(msg); return true;
    case 'artefact':    callbacks.onArtefact && callbacks.onArtefact(msg); return true;
    case 'llm_error':   callbacks.onError && callbacks.onError(msg.message); return true;
    case 'done':        callbacks.onDone && callbacks.onDone(); return true;
    default:            return false;
  }
}
