Starting points for OpenResty (nginx + LuaJIT) — not exhaustive. Lua running inside nginx worker processes; every request flows through phase-based handlers (`access_by_lua`, `content_by_lua`, `rewrite_by_lua`, etc.). Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`ngx.req.get_uri_args()`, `ngx.req.get_post_args()`, `ngx.var.arg_name`, `ngx.req.read_body()` + `ngx.req.get_body_data()`, `ngx.req.get_headers()`, `ngx.var.cookie_name`, `ngx.var.http_header_name`, `ngx.var.remote_addr` (client IP — spoofable via forwarded headers if trusted).

`ngx.var.*` is a wide surface — ANY nginx variable, including those injected by upstream proxies that might not be sanitising.

## Sinks

**Dynamic code / `loadstring`**
- `loadstring(ngx.var.arg_code)()` — RCE.  Never eval user input.
- `ngx.eval_string` (some modules) — same class.

**Shell via lua-resty-shell**
- `shell.run(user_cmd)` — shell command execution in a worker; RCE.
- Plain `os.execute` is often disabled by `disable_unsafe_functions` convention, but check the build.

**Upstream selection / SSRF**
- `ngx.location.capture(user_uri)` — internal subrequest.  If `user_uri` is attacker-derived, attacker can hit other internal endpoints.
- `ngx.location.capture_multi({ {uri1}, {uri2} })` — same across multiple URIs.
- `ngx.var.upstream_url` dynamic upstream selection based on `Host` / path — bypass of upstream allowlist.
- `ngx.redirect(user_url)` — open redirect unless host-allowlisted.

**Header / body injection**
- `ngx.header[user_name] = user_value` — CRLF injection if `user_name` or `user_value` contains `\r\n`.
- `ngx.say(user_html)` — raw body output; XSS in HTML responses.
- `ngx.print(user)` — same.

**SQL (lua-resty-mysql / pgmoon / lua-resty-redis)**
- `db:query(ngx.quote_sql_str(user))` — `ngx.quote_sql_str` is escape, NOT parameterisation; it helps for `varchar` contexts but not identifiers.  Use parameterised queries when possible.
- `db:query("SELECT ... '" .. user .. "'")` — SQLi; `ngx.quote_sql_str` at least escapes `'`.
- pgmoon: `db:query('... $1', user)` is parameterised.

**JWT / auth (lua-resty-jwt, lua-resty-openidc)**
- `jwt:verify(secret, token)` with a hardcoded `secret` — key leak = token forgery.
- `lua-resty-openidc` with `opts.discovery_document_expires_in = 0` — disables caching; doesn't fix anything but amplifies IdP load; not security.
- `lua-resty-openidc`'s `token_endpoint_auth_method = "client_secret_post"` with a committed client secret.

**Cache / shared dict**
- `ngx.shared.mydict:set(key, value, 60)` — shared memory; attacker-controllable keys can exhaust or poison the shared dict.
- `ngx.shared.mydict:get(user_key)` — attacker chooses which cached value to retrieve; IDOR-class concern if per-user entries exist.

**nginx configuration entries (out of scope if purely config, worth flagging)**
- `location /admin { allow 10.0.0.0/8; deny all; }` — missing `deny all` before `allow` rules = allow-all.
- `resolver 8.8.8.8;` used by `ngx.location.capture` dynamic upstream — public DNS = upstream-lookup leakage.
- `proxy_pass $backend;` where `$backend` is set from `ngx.var.arg_*` — SSRF via query param choosing the upstream.

**`access_by_lua` auth bypass patterns**
- `if not auth_ok then ngx.exit(401) end` — fine.  Missing the `return` after `ngx.exit` in some contexts continues execution.  Use `ngx.exit(ngx.HTTP_UNAUTHORIZED); return` or let the phase handler return implicitly.
- `if ngx.var.http_x_api_key == "hardcoded" then ... end` — committed auth token.

**WAF / rate limit bypass**
- `lua-resty-limit-traffic` keyed on `ngx.var.remote_addr` — if behind a proxy forwarding `X-Forwarded-For`, attacker spoofs the header to evade rate limiting (unless `real_ip_header X-Forwarded-For` is set and trusted proxies are restricted).

**Log sinks**
- `ngx.log(ngx.ERR, user)` — log injection if `user` contains `\r\n`.  Sanitize before logging.

## Tree-sitter seeds (lua, OpenResty-focused)

```scheme
; ngx.* calls
(function_call
  name: (dot_index_expression
    table: (identifier) @t
    field: (identifier) @fn)
  (#eq? @t "ngx")
  (#match? @fn "^(say|print|redirect|exit|exec|location|eval_string|var|req|shared|log|header)$"))

; lua-resty-* dotted access
(function_call
  name: (dot_index_expression
    table: (dot_index_expression table: (identifier) @root field: (identifier) @sub)
    field: (identifier) @fn)
  (#eq? @root "ngx"))
```

`tree-sitter-lua` may or may not be in-tree for `ast_query`; always `ast_describe` on a representative snippet before structural queries.
