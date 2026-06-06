Starting points for Slim (PHP micro-framework) — not exhaustive. PSR-7 request/response; no built-in ORM or templating. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`$request->getQueryParams()`, `$request->getParsedBody()`, `$request->getAttribute('id')` (route args via middleware), `$request->getHeaderLine('H')`, `$request->getCookieParams()`, `$request->getUploadedFiles()`, `$request->getBody()->getContents()`.

## Sinks

**SQL (PDO / Doctrine DBAL / raw)**
- `$pdo->query("SELECT ... '" . $user . "'")` — SQLi; use `prepare` + `bindValue`.
- Doctrine DBAL `$conn->executeQuery("... $user")` — raw; use `?` placeholders.

**Command execution**
- `system($request->getQueryParams()['cmd'])` — RCE.
- `exec`, `shell_exec`, `passthru`, backticks — same.

**Deserialization**
- `unserialize($request->getBody()->getContents())` — RCE via POP chains.
- Slim's session middleware (`Slim\Session` variants) — check the session serializer; `php_serialize` is PHP's native `serialize`, i.e. unserialize-on-read.

**File / path**
- `$response->getBody()->write(file_get_contents($userPath))` — traversal + direct disclosure.
- `$uploaded->moveTo($dir . $uploaded->getClientFilename())` — attacker filename; use basename + allowlist.

**Redirect**
- `$response->withHeader('Location', $userUrl)->withStatus(302)` — open redirect.
- `$response->withRedirect($userUrl)` — same.

**Routing**
- `$app->any('/.*', handler)` wildcard catch-all — if unauthenticated, attackers reach every subroute with no differentiation.
- Route patterns with user-controlled placeholders: `$app->get('/{path:.*}', fn... => file_get_contents("files/" . $args['path']))` — traversal.

**Middleware composition**
- Auth middleware added AFTER specific routes via `$app->addMiddleware(...)` doesn't wrap routes registered BEFORE it in some Slim versions — middleware order matters.  Look for the add-order.
- `$app->add(new AuthMiddleware())` globally, but `$app->get('/public', ...)->addMiddleware(new BypassAuth())` — middleware chains compose; a bypass middleware on a specific route can disable auth.

**Error handler**
- `$app->addErrorMiddleware(true, true, true)` — first `true` is `displayErrorDetails`.  Production with `true` leaks stack traces.
- Custom error handlers writing `$throwable->getMessage()` to the response — PII / secret leak via exception text.

**CORS (via tuupola/cors-middleware or similar)**
- `"origin" => "*", "credentials" => true` — credentialed wildcard.

**Templating (if integrated)**
- Twig / Plates / Blade via third-party — each has its own escape-bypass idiom (`raw`, `{!!...!!}`, etc.); refer to the relevant framework sheet.

## Tree-sitter seeds (php, Slim-focused)

```scheme
; $app->get / ->post / ->any / ->group
(member_call_expression
  object: (variable_name (name) @o)
  name: (name) @m
  (#eq? @o "app")
  (#match? @m "^(get|post|put|delete|patch|options|any|map|group|add|addMiddleware|addErrorMiddleware)$"))

; $request->get* / $response->with*
(member_call_expression
  name: (name) @m
  (#match? @m "^(getQueryParams|getParsedBody|getAttribute|getHeaderLine|getCookieParams|getUploadedFiles|getBody|withHeader|withStatus|withRedirect|withBody)$"))
```
