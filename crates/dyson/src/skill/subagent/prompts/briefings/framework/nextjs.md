Starting points for Next.js — not exhaustive. Combines the Express-style API-route surface with React Server Components (RSC) / Server Actions — both have sharp edges. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

**Pages Router (`pages/api/*`)**
- `req.query`, `req.body`, `req.cookies`, `req.headers`.

**App Router (`app/**/route.ts`, Route Handlers)**
- `request.nextUrl.searchParams`, `await request.json()`, `await request.formData()`, `request.cookies`, `request.headers`.

**Server Components**
- Page / layout props: `params` (URL segment values), `searchParams` (query string).

**Server Actions (`'use server'` functions)**
- Every argument is attacker-controlled.  The client can craft arbitrary `FormData` and call any exported server action via the `$ACTION_ID_` / `$ACTION_REF_` wire protocol.

## Sinks

**RSC reply / Server Action prototype-walk (see lang/javascript.md RSC section)**
- Form posts to a `'use server'` action flow through `ReactFlightReplyServer.js` in the `react-server-dom-*` bundler.  Reviewer should treat the serialized reference path (`"1:constructor:..."`) as attacker-controlled, walking `value[path[i]]` without a blocklist = prototype-walk RCE.
- Server Actions registered in a trusted manifest still accept any `$ACTION_ID_<hex>` key from the FormData — flag actions that take user input without per-request authorization.

**SQL**
- Prisma `db.$queryRawUnsafe(\`SELECT ... ${user}\`)` — SQLi.  `db.$queryRaw\`SELECT ... ${user}\`` (tagged template) IS parameterised and safe.
- Drizzle `db.execute(sql.raw(\`... ${user}\`))` — raw; use `sql\`... ${user}\`` (tagged).
- `pg.query(\`... ${user}\`)` — string concat.

**File / path**
- `fs.readFile(path.join(process.cwd(), req.query.f))` — traversal.  `path.normalize` doesn't anchor.
- `NextResponse.json(fs.readFileSync(req.query.path))` — same.
- `next.config.js` `rewrites()` with user-derived `destination` — unintended proxy.

**Redirect**
- `redirect(user_url)` (from `next/navigation`) — open redirect if `user_url` is external and attacker-controlled.
- `NextResponse.redirect(request.nextUrl.searchParams.get('next'))` — open redirect.
- `return { redirect: { destination: user_url, permanent: false } }` in `getServerSideProps`.

**XSS**
- `dangerouslySetInnerHTML={{ __html: user }}` — always XSS if `user` isn't pre-sanitized (DOMPurify or similar).
- Markdown rendering libs with `sanitize=false` — XSS.
- `NextResponse.redirect(user_url, { ... })` with `javascript:` scheme — XSS (some environments still open this).

**Command execution**
- `child_process.exec(req.query.cmd)` in a route handler — RCE.
- `new Worker(user_script_path)` — if path controlled, worker runs attacker code.

**Auth / middleware**
- `middleware.ts` with `matcher` paths that don't cover every sensitive route — auth gap.  List the matchers and cross-reference with the protected routes.
- `getServerSession(authOptions)` returning `null` on a route that doesn't check it — unauth access.
- NextAuth / Auth.js with `jwt.secret = 'dev'` — signing-key leak.
- `authOptions.callbacks.jwt` / `session` callbacks that trust attacker-supplied `token.role` without verifying.

**Server Actions specifics**
- Every `'use server'` exported function is reachable from ANY client on the site via the RSC protocol — there is no route-level auth gate.  Auth must be IN the action body: `const session = await auth(); if (!session) throw ...`.
- `formData.get('field')` returns `FormDataEntryValue | null` — a `File` instance on upload, string otherwise.  Type-narrowing mistakes (`const s: string = formData.get('x') as string`) paper over a `File` that the action then tries to use as a string.

**Images / SSRF via `next/image`**
- `next.config.js` `images.remotePatterns` / `domains` — without a strict allowlist, `/_next/image?url=http://internal-service/` is SSRF through the Next.js image optimizer.
- `images.dangerouslyAllowSVG: true` — permits SVG → XSS via `<script>` inside SVG.

**API route exposure**
- `pages/api/*.ts` — every file is a public endpoint.  A debug-only `pages/api/internal.ts` committed is a finding.
- Route Handlers with no `export` of HTTP methods other than expected: double-check that `OPTIONS` / `HEAD` aren't falling through to a default handler that serves unintended data.

**Environment / secrets**
- `NEXT_PUBLIC_*` env vars are inlined into the CLIENT bundle.  `NEXT_PUBLIC_API_KEY` = secret leaked to every browser.  Flag any `NEXT_PUBLIC_*` that looks like a credential.
- `.env.local` committed — don't.  Check `.gitignore`.

## Tree-sitter seeds (typescript / javascript)

```scheme
; 'use server' directive — marks a Server Action
(string (string_fragment) @s (#eq? @s "use server")) @directive

; Prisma / Drizzle raw query entries
(call_expression function: (member_access_expression property: (property_identifier) @m)
  (#match? @m "^(\\$queryRawUnsafe|\\$executeRawUnsafe|queryRawUnsafe|executeRawUnsafe)$"))

; NextResponse.redirect / .rewrite / redirect() from next/navigation
(call_expression function: (member_expression property: (property_identifier) @m)
  (#match? @m "^(redirect|rewrite|json|next)$"))
```
