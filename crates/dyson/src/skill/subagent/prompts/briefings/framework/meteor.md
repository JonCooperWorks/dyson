Starting points for Meteor (full-stack JS) — not exhaustive. Realtime data syncing + Methods (server RPCs) + Publications (subscriptions).  Legacy apps are common.  Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)

**Methods** (server RPCs) — `Meteor.methods({ name: function (...args) { ... } })`:
- Every argument to a Method is attacker-controlled.  The client calls `Meteor.call('name', arg1, arg2)` with any values.
- `this.userId` inside a Method — server-verified; safe to trust as identity if `this.userId` is non-null.

**Publications** (`Meteor.publish('name', function (...args) {...})`):
- Args are attacker-controlled.  Return values determine what data ships to the client.

**HTTP endpoints** (if using Iron Router / Meteor WebApp):
- `req.body`, `req.query`, `req.headers` — standard Node request shape.

## Sinks

**Mongo collection operations (the #1 Meteor vuln class)**
- `Collection.update({ _id: docId }, { $set: user_doc })` — if `user_doc` is `request.params.update`, attacker sets ANY field including `$set`, `$inc`, etc.  Use schema validation (`check()` or `SimpleSchema`).
- `Collection.allow({ insert: () => true, update: () => true, remove: () => true })` — client-side inserts/updates with trivial server-side approval.  **CRITICAL**: this gives clients arbitrary collection writes.
- `Collection.deny(...)` is the counterpart; absence of `deny` rules with a permissive `allow` = client-side full CRUD.

**`check()` / `Match` arguments**
- Methods that receive arbitrary args without `check(arg, Match.X)` accept any JSON.  The check is how you enforce shape server-side.
- `check(arg, Object)` — accepts ANY object; doesn't validate shape.  Use `check(arg, { name: String, email: String })` with an explicit shape.
- `Match.Any` — accepts anything; rare legitimate use.

**Methods that mutate without auth**
- Method bodies that run `Collection.update(...)` without checking `this.userId` — anonymous callers can mutate.
- `this.unblock()` early in a Method — parallelises execution but also bypasses per-user rate limiting downstream.

**Publications returning too much**
- `Meteor.publish('users', () => Users.find())` — publishes ALL users' ALL fields including password hashes and session tokens.  Use `Users.find({}, { fields: { username: 1, profile: 1 } })` explicitly.
- `Meteor.publish('user', function (id) { return Users.find({ _id: id }) })` — publishes the full user doc including `services.password.bcrypt`.  Always restrict fields.

**Eval / dynamic code**
- Meteor's build system sometimes uses `new Function(code)` — not user-facing usually.
- `eval(userInput)` in a Method — RCE.

**Server-side render / templates**
- Blaze templates: `{{{user}}}` unescaped; `{{user}}` escaped.

**Legacy password auth**
- `Accounts.createUser({ username, password })` — Meteor's Accounts package stores bcrypt hashes server-side.  Custom auth flows that store plain passwords or use MD5 are findings.
- `Accounts.config({ sendVerificationEmail: false })` + self-service signup = unverified accounts.

**OAuth secrets**
- `ServiceConfiguration.configurations.upsert({ service: 'facebook' }, { $set: { secret: 'committed-literal' } })` — client secret committed to source.

## Tree-sitter seeds (javascript, Meteor-focused)

```scheme
; Meteor.methods / Meteor.publish / Meteor.call
(call_expression
  function: (member_expression
    object: (identifier) @m
    property: (property_identifier) @fn)
  (#eq? @m "Meteor")
  (#match? @fn "^(methods|publish|subscribe|call|apply|startup|isServer|isClient|userId|user)$"))

; check() calls — presence absence on method args is the finding
(call_expression
  function: (identifier) @c
  (#eq? @c "check"))

; Collection.allow / .deny / .update / .insert / .remove with direct arg passthrough
(call_expression
  function: (member_expression
    property: (property_identifier) @m)
  (#match? @m "^(allow|deny|update|insert|remove|upsert|find|findOne)$"))
```
