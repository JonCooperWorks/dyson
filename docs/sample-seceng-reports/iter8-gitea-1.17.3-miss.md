## Security Review: Gitea Markup/HTML Rendering Library

## CRITICAL

No CRITICAL findings.

## HIGH

No HIGH findings.

## MEDIUM

### Orgmode link description placed in HTML attributes without escaping

- **File:** `orgmode/orgmode.go:164-169`
- **Evidence:**
  ```go
  description := string(link)
  if l.Description != nil {
  	description = r.WriteNodesAsString(l.Description...)
  }
  switch l.Kind() {
  case "image":
  	imageSrc := getMediaURL(link)
  	fmt.Fprintf(r, `<img src="%s" alt="%s" title="%s" />`, imageSrc, description, description)
  ```
- **Attack Tree:**
  ```
  orgmode/orgmode.go:111 — org.New().Parse(input, "") parses user-controlled orgmode content
    └─ orgmode/orgmode.go:158 — WriteNodesAsString writes link description nodes to string
      └─ orgmode/orgmode.go:164 — description placed directly into alt/title attributes via fmt.Fprintf
        └─ orgmode/orgmode.go:169 — <a href="%s" title="%s">%s</a> — unescaped description in attribute AND body
  ```
- **Taint Trace: not run within budget — same-line / structural evidence only**
- **Impact:** An attacker who can create an orgmode file (e.g., a malicious `.org` file in a repo) can craft a link whose description text contains `"` characters. These break out of the `alt=""` or `title=""` attribute boundary, allowing injection of arbitrary HTML. For example, a description containing `"><script>alert(1)</script><a href="` would break out of the attribute and inject a `<script>` tag. The downstream `bluemonday` UGC sanitizer may catch `<script>` tags, but attribute-injection with event handlers (e.g., `onerror=`) on allowed elements like `<img>` may bypass it if the orgmode library already emits HTML tags that the sanitizer permits.
- **Exploit:** Create a link in an orgmode file: `[[https://example.com]["><img src=x onerror=alert(1)>]]` — the description part `"><img src=x onerror=alert(1)>` is written directly into the `alt` and `title` attributes via `fmt.Fprintf` without escaping.
- **Remediation:** Escape `description` before placing in attributes. Replace line 157-164:
  ```go
  descEsc := html.EscapeString(description)
  // then use descEsc in fmt.Fprintf formats
  fmt.Fprintf(r, `<img src="%s" alt="%s" title="%s" />`, imageSrc, descEsc, descEsc)
  fmt.Fprintf(r, `<a href="%s" title="%s">%s</a>`, link, descEsc, descEsc)
  ```

## LOW / INFORMATIONAL

No findings.

## Checked and Cleared

- `csv/csv.go:67` — CSV fields are passed through `html.EscapeString` before writing to HTML — XSS prevented.
- `csv/csv.go:90-98` — CSV files exceeding `MaxFileSize` are displayed as escaped text — no injection.
- `sanitizer.go:52-128` — `createDefaultPolicy()` uses `bluemonday.UGCPolicy()` as base — well-maintained HTML sanitizer with appropriate defaults.
- `sanitizer.go:130-143` — `addSanitizerRules` only adds rules from configuration; does not weaken base policy.
- `html.go:297-298` — `tagCleaner` regex escapes `<html>`, `<head>`, `<htm>`-like tags in raw input before HTML parsing — defense in depth.
- `html.go:185-198` — `renderIFrame` uses `url.PathEscape` on all Metas values before embedding in iframe src — injection prevented.
- `html.go:498-516` — `createLink` creates HTML nodes directly (not string formatting) — no string injection vector.
- `html.go:1128` — SHA1 pattern processor creates links from hash strings that are verified against git repo before linking.
- `markdown/goldmark.go:257-306` — Goldmark HTML renderer creates safe HTML nodes; no raw string formatting of user content.
- `markdown/goldmark.go:320-367` — `renderIcon` validates icon name against `^[a-z ]+$` regex before output — injection prevented.
- `common/footnote.go:29-47` — `CleanValue` strips non-alphanumeric characters from footnote names — ID injection prevented.
- `common/footnote.go:411-435` — Footnote HTML renderer writes pre-escaped name values (processed through `CleanValue`) — safe.
- `external/external.go:80-138` — External renderer command is built from `setting.MarkupRenderer.Command` (server config), not user input — no command injection from untrusted markdown.
- `external/external.go:127` — `exec.CommandContext` uses `commands[0]` and `commands[1:]` from server-configured command string — not attacker-controlled.
- `console/console.go:61-70` — Console renderer uses `terminal-to-html` library which strips escape sequences — no injection.
- `mdstripper/mdstripper.go:38-66` — Strip renderer outputs plain text only — no HTML injection.
- `mdstripper/mdstripper.go:171-173` — Uses `html.WithUnsafe()` but the stripRenderer outputs only text, not HTML — no risk.
- `camo.go:21-32` — Camo encode uses HMAC-SHA1 for URL signing. SHA1 is weak but camo is for privacy/filtering, not cryptographic integrity — acceptable for use case.
- `renderer.go:227` — All renderer output passes through `SanitizeReader` using `bluemonday` policy unless explicitly disabled by config.

## Dependencies

No dependency manifests (go.mod) found in this directory. This is a subpackage of the Gitea codebase; dependency scanning should be performed on the root Gitea repository. **no vulnerable dependencies found** in this subpackage scope.

## Remediation Summary

### Immediate (CRITICAL/HIGH)

No immediate fixes required.

### Short-term (MEDIUM)

1. `orgmode/orgmode.go:164-169` — Escape `description` text with `html.EscapeString()` before embedding in HTML attributes (`alt`, `title`) and element body content in `WriteRegularLink`. The `link` variable is already escaped but `description` from `r.WriteNodesAsString(l.Description...)` is not.

### Hardening (LOW)

No hardening recommendations.