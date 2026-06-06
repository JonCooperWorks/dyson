Starting points for tRPC (TypeScript RPC over HTTP) — not exhaustive. tRPC's safety rests entirely on whether `input()` is called with a Zod/Valibot/Yup/Arktype schema.  Missing `input()` = untyped attacker input into the procedure. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

**Mutation / query `input`** — the argument passed client-side.  If a procedure uses `.input(z.object({...}))`, Zod rejects unknown fields (with `.strict()`).  Without `.input(...)`, `input` is `unknown` and often `any`-cast in the resolver body.

**Context** — `createContext` typically extracts session/user from the request.  Attacker controls request headers / cookies; the context builder MUST validate (verify JWT, check session store).  `ctx.user = req.headers['x-user-id']` trusting a client-provided header IS the bug.

## Sinks

**Procedure without `input()`**
- `publicProcedure.mutation(async ({ ctx, input }) => { ... })` — `input` is untyped; all shape validation is skipped.  Downstream `input[user_key]` walks or `Object.assign(dbRow, input)` is prototype-walk / mass-assignment.
- `.input(z.any())` / `.input(z.record(z.any()))` — validates that it's an object, not its shape.  Same class of issue.

**Mass assignment**
- `ctx.db.user.update({ where: { id: ctx.user.id }, data: input })` — if `input` isn't constrained to a specific set of fields, attacker sets `admin: true`, `role: 'ADMIN'`, `emailVerified: new Date()`.
- Use `.input(z.object({ name: z.string() }).strict())` — `.strict()` rejects extras; without it Zod silently drops unknown keys BUT downstream `input as User` TypeScript cast lies.

**Auth middleware gaps**
- `publicProcedure` vs `protectedProcedure` — every sensitive mutation MUST be on `protectedProcedure` (or equivalent `.use(isAuthed)` middleware).  Look for auth-sensitive data mutations on `publicProcedure`.
- Per-resource authorization inside the procedure: `ctx.db.post.update({ where: { id: input.id }, ... })` — if `input.id` is user-supplied and there's no `.where({ id: input.id, authorId: ctx.user.id })` scoping, IDOR.

**Server-side data / SQL**
- Procedures calling `$queryRawUnsafe(\`... ${input.x}\`)` on Prisma — SQLi.  Use `$queryRaw\`... ${input.x}\`` tagged template.
- Drizzle `db.execute(sql.raw(\`... ${input.x}\`))` — raw; use `sql\`... ${input.x}\``.

**Error leakage**
- `errorFormatter` in `initTRPC.create({ errorFormatter })` — the default includes `stack` and `cause` on non-production.  `NODE_ENV !== 'production'` check is relied on; if incorrectly configured, stack traces ship to clients.
- Custom errors with PII / secrets in the message field — the default error formatter includes `message`.

**HTTP adapter / batch**
- Default `createNextApiHandler` / `fetchRequestHandler` / `createHTTPHandler` supports batching: many procedures in one HTTP call.  Rate limits must account for the batch count, not just the request count.  (Rate-limit concerns out of scope per rules, but relevant context.)
- `allowOutsideOfServer: true` — CORS-adjacent; permits calls outside normal origins.

**WebSocket subscriptions**
- `subscription` procedures over `@trpc/server/adapters/ws` — auth happens at connection time via `createContext`, then every subscription inherits.  A subscription that depends on `input` for authorization AFTER the connection is opened can be ambushed by reconnect with a different `input`.

**Links / client concerns leaking to server trust**
- `httpBatchLink` + custom `headers: async () => ({ authorization: '...' })` — client sets headers; server MUST NOT trust anything the client sends as identity, even if the header is named `authorization`.

## Tree-sitter seeds (typescript, tRPC-focused)

```scheme
; Procedure chain: .query(...) / .mutation(...)
(call_expression
  function: (member_expression
    property: (property_identifier) @m)
  (#match? @m "^(query|mutation|subscription)$"))

; .input(schema) — presence check
(call_expression
  function: (member_expression
    property: (property_identifier) @m)
  (#eq? @m "input"))

; .use(middleware) on tRPC builder
(call_expression
  function: (member_expression
    property: (property_identifier) @m)
  (#eq? @m "use"))
```
