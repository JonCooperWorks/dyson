// Paint the "sign-in failed" frame when auth bootstrap rejects.
//
// Built with DOM nodes + textContent: the error message can carry text
// from the IdP / discovery response, so it must never be parsed as HTML.
export function renderBootError(root, err) {
  if (!root) return;
  const doc = root.ownerDocument || document;
  const frame = doc.createElement('div');
  frame.style.cssText = 'padding:32px;font-family:system-ui;color:#eee;';
  const title = doc.createElement('h2');
  title.textContent = 'Sign-in failed';
  const message = doc.createElement('p');
  message.textContent = String((err && err.message) || err);
  const hint = doc.createElement('p');
  hint.textContent = 'Check that your IdP issuer is reachable and the client_id is registered.';
  frame.append(title, message, hint);
  root.replaceChildren(frame);
}
