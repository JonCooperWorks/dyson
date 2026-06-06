Starting points for GraphQL servers (Apollo, Yoga, mercurius, @nestjs/graphql, graphene-python, etc.) — not exhaustive. GraphQL collapses many HTTP endpoints into one schema; the usual web sinks still apply but the attack surface has GraphQL-specific shapes. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

- **Query body**: arbitrary GraphQL documents over POST.  The attacker picks which resolvers fire and in what order.
- **Variables**: typed per-field input.  Shape enforced by the schema.
- **Operation name**: `operationName` selects which operation in a multi-op query to run.
- **Extensions / directives**: custom directives can pull user data into resolver context.

## Sinks and anti-patterns

**N+1 auth-bypass**
- The gold bug: a `Query.user(id)` resolver checks the current user's permissions, but `Query.user(id).posts` or `Query.user(id).paymentMethods` field resolvers DON'T re-check.  An attacker who can reach the top-level type via any path then queries sensitive nested fields without auth.
- Rule: every field that returns or filters sensitive data enforces its own authorization; don't rely on parent-resolver checks.

**Introspection on production**
- `introspection: true` (Apollo) / `graphiql: true` (Yoga) — exposes the full schema to anonymous clients.  For a private API, the schema IS sensitive (reveals internal type names, hidden fields, operation surface).  Disable in production.

**Depth / complexity attacks (normally DoS → out of scope)**
- Arbitrarily nested queries (`user { posts { author { posts { author { ... } } } } }`) — DoS via recursive fanout.  Only a finding if it yields memory corruption or priv-esc (per rules).  Most projects use `graphql-depth-limit` / cost-analysis; absence is worth a LOW informational line.

**Batching + mutations**
- Batched requests (multiple operations in one HTTP call) bypass per-request rate-limits if the limiter counts HTTP requests not operations.
- Aliases let an attacker submit 100 copies of the same mutation in one request (`m1: mutation1(...) m2: mutation1(...) ...`) — auth operations (password-reset, login) need per-operation counting.

**Query injection into SQL / NoSQL**
- Resolver code: `return db.raw(\`SELECT * FROM users WHERE name = '${args.name}'\`)` — same SQLi as any other framework; happens inside resolvers.
- Mongoose `.find(args.filter)` — operator injection (`{ $ne: null }`).

**Mass assignment in mutations**
- `updateUser(input: UserInput!)` where `UserInput` has every field of the User schema — attacker sets `role: ADMIN`, `emailVerified: true`.  Use narrow input types per mutation; don't reuse the full type.
- `input: JSON` scalar types accepting arbitrary objects — bypass of type-level validation entirely.

**Custom scalars**
- `DateTime`, `Email`, `URL` scalars that don't reject malformed input at parse time leak validation to resolver code.  Attackers pass `http://internal-service/` where a `URL` scalar was supposed to constrain — SSRF.
- `JSONObject` / `JSON` scalars — same prototype-walk concerns as any untyped JSON.

**Error leakage**
- Apollo Server default returns stack traces in `error.extensions` when `NODE_ENV !== 'production'`.  Mis-set env → stack leaks.
- Resolver exceptions with user data in the message → PII in GraphQL error response.

**Directives as side channels**
- Custom `@auth(role: "admin")` directive enforced at the SCHEMA layer, not the resolver.  If a directive is dropped from the schema definition OR the directive implementation has a bypass (returning `true` on null context), authorization is off for that field.

**Federation / subgraph trust**
- Federated subgraphs trust `_entities` calls from the gateway.  A misconfigured gateway accepting federation calls from unauth clients bypasses per-subgraph auth.
- Subgraph resolvers assume `context.user` was set by the gateway; if the gateway forwards a user header without verifying, the subgraph trusts a forged header.

**Subscription auth**
- WebSocket subscription connections authenticate at `connectionParams` time.  Per-operation auth checks MUST happen inside each subscription resolver; relying on connection-time auth alone lets a client subscribe then consume events meant for a different user.

## Tree-sitter seeds (javascript / typescript, GraphQL-focused)

```scheme
; gql`...` template literal or gql(`...`) call — defines the schema / op
(call_expression function: (identifier) @f (#eq? @f "gql"))

; Resolver map keys: Query / Mutation / Subscription
(property_signature (property_identifier) @p
  (#match? @p "^(Query|Mutation|Subscription)$"))

; Apollo Server / graphql-yoga instantiation
(new_expression constructor: (identifier) @c
  (#match? @c "^(ApolloServer|YogaServer)$"))
```

For schema-first servers, `search_files` for `type Query {` / `type Mutation {` blocks and cross-reference against per-field auth directives.
