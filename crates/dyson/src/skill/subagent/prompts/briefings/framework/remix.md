Starting points for Remix — not exhaustive. Server loaders + form actions, similar shape to SvelteKit. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

**`loader`** (in `routes/*.tsx`):
- Receives `{ request, params, context }`.  `request.url`, `params.id`, `request.headers`, cookies via `context.session` or `request.headers.get('Cookie')`.

**`action`**:
- Receives `{ request, params }`.  `await request.formData()` is attacker-controlled.

## Sinks

**SQL (Prisma / Drizzle)**
- `db.$queryRawUnsafe(\`... ${formData.get("q")}\`)` — SQLi; use `$queryRaw\`...\`` tagged template.
- `db.execute(sql.raw(\`... ${user}\`))` — Drizzle raw; use tagged-template.

**Redirect**
- `return redirect(url.searchParams.get("next") ?? "/")` — open redirect.  Always allowlist.
- `throw redirect(302, userUrl)` in a loader — same.

**Session / auth**
- `createCookieSessionStorage({ cookie: { secrets: ["dev"] } })` — hardcoded signing secret.  `secrets` is an ARRAY to support rotation; the first element must be env-sourced.
- `authenticator.isAuthenticated(request, { failureRedirect: "/login" })` missing on a mutation action → anonymous mutation.
- Custom `getUserFromRequest(request)` that parses a header/cookie without verifying the signing cookie = forged identity.

**File / path**
- `unstable_createFileUploadHandler({ directory: userDir })` — attacker-derived directory at handler creation.  File names from `NodeOnDiskFile.name` are attacker-controlled; use basename + anchor.
- `fs.readFile(userPath)` in a loader — traversal.

**Environment / secrets**
- `process.env.SECRET` in a loader is fine (server-only).  Importing a `.server.ts` module from a route component transitively is fine.  Importing a non-`.server.ts` module that reads `process.env.SECRET` into a const is fine IF no client code imports that module — Remix's tree-shake splits server from client, but `.client.ts` / lack of `.server.ts` can leak.

**Meta / links / scripts**
- `meta: () => [{ tagName: "script", children: userJs }]` — literal JS injection.
- `links: () => [{ rel: "stylesheet", href: userUrl }]` — linked stylesheet can be an attacker URL; risk depends on CSP.

**XSS (React output)**
- `dangerouslySetInnerHTML={{ __html: user }}` — XSS if `user` isn't sanitized.
- `<a href={userUrl}>` with `javascript:` scheme → XSS on click (browsers mitigate; don't rely).

**Deferred / streaming loaders**
- `defer({ slow: slowPromise })` — streamed; if `slowPromise` rejects with a user-containing message, the error boundary sees it.  Sensitive errors leak unless explicitly caught.

**Nested routes / parent loader data**
- `useLoaderData()` returns only the current route's data.  `useRouteLoaderData("root")` accesses a parent's loader — check parent loaders don't leak data that shouldn't reach this route.

**`remix-auth`**
- Strategy callbacks receiving `profile` from OAuth providers — the attacker controls their OAuth account's `email` / `name` fields.  Downstream `upsert({ email: profile.email })` without verifying `email_verified` (Google) or `verified_email` (GitHub) = account takeover via OAuth-provider-reported email.

## Tree-sitter seeds (typescript, Remix-focused)

```scheme
; export const loader / action / meta / links
(export_statement (lexical_declaration
  (variable_declarator
    name: (identifier) @m
    (#match? @m "^(loader|action|meta|links|headers|shouldRevalidate|ErrorBoundary|CatchBoundary)$"))))

; redirect / json / defer
(call_expression
  function: (identifier) @f
  (#match? @f "^(redirect|json|defer|unstable_defer)$"))
```
