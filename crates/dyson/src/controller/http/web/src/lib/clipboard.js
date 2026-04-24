/* Dyson — clipboard helper.
 *
 * The modern clipboard API (`navigator.clipboard.writeText`) is the
 * right call, but browsers gate it on a secure context — and Dyson is
 * commonly served over plain HTTP on a Tailscale / LAN address, where
 * `navigator.clipboard` is `undefined`.  The legacy textarea +
 * `execCommand('copy')` dance still works in that context, so the
 * helper falls through to it before giving up.
 *
 * Callers want a boolean so they can flip a "copied" pip in the UI
 * for ~1 s; swallowing the error (clipboard denied, headless, etc.)
 * and returning `false` is the contract.
 *
 * Before this file lived here, the dance was copy-pasted across
 * turns.jsx, panels.jsx, and views-secondary.jsx — three places to
 * patch when clipboard permissions changed.
 */
export async function copyToClipboard(text) {
  if (!text) return false;
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    document.execCommand('copy');
    document.body.removeChild(ta);
    return true;
  } catch (_) {
    return false;
  }
}
