Starting points for Cowboy (Erlang HTTP server) — not exhaustive. BEAM-level HTTP/1.1, HTTP/2, and WebSocket. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`cowboy_req:binding(Key, Req)` (URL segments), `cowboy_req:parse_qs(Req)` (query), `cowboy_req:read_body(Req)` (body bytes), `cowboy_req:header(Name, Req)`, `cowboy_req:parse_cookies(Req)`.

## Sinks

**Deserialization via `binary_to_term`**
- `binary_to_term(Body)` without `[safe]` — RCE via crafted BERT terms.  ALWAYS `binary_to_term(Body, [safe])`.
- `Plug.Crypto.non_executable_binary_to_term/2` (Elixir) — the correct wrapper, but Cowboy-native Erlang handlers often use raw `binary_to_term`.

**Dynamic atom creation**
- `list_to_atom(binary_to_list(Body))` — atom exhaustion DoS (atoms not GC'd; default ~1M).  Use `list_to_existing_atom` / `binary_to_existing_atom`.

**Dynamic dispatch**
- `apply(Mod, Fn, Args)` with `Mod` / `Fn` as user-derived atoms — arbitrary exported function invocation.  Classical RCE primitive.

**SQL**
- Raw SQL via epgsql / mysql-otp: `epgsql:squery(C, "... " ++ binary_to_list(User))` — concat: SQLi.  Use `epgsql:equery(C, "... $1", [User])`.

**Code loading**
- `code:load_file(UserMod)`, `code:load_binary(UserMod, File, UserBin)` — RCE if `UserBin` is attacker-controlled bytecode.

**File / path**
- `file:read_file(UserPath)` — traversal.
- `cowboy_static`'s dir handler with user-derived base path — attacker-derived serve root.

**Redirect**
- `cowboy_req:reply(302, #{<<"location">> => UserUrl}, Req)` — open redirect.

**XSS**
- `cowboy_req:reply(200, #{<<"content-type">> => <<"text/html">>}, UserHtml, Req)` — raw HTML body.
- ErlyDTL / egotpl templates: `{{ user|safe }}` bypasses escape.

**WebSocket (cowboy_websocket)**
- `websocket_handle({text, Data}, State)` — `Data` is attacker-controlled text frame.  Feeding into `binary_to_term` without `[safe]` = RCE.  Feeding into `jsx:decode` + walking keys = prototype-walk analogue.
- `websocket_init(State)` without auth check — anonymous WebSocket clients.

**Process / mailbox**
- Handlers running as short-lived processes — a handler spawning a long-lived process carrying request data without termination → resource leak + possibly data accessible to other requests.

**Crypto**
- `crypto:strong_rand_bytes/1` is correct; `random:uniform` is NOT cryptographic.
- `crypto:hash(md5, _)` / `sha` for password hashing — use `crypto:pbkdf2_hmac` or a library.
- `X =:= Y` comparison on MACs — timing-unsafe.  Use a constant-time compare helper.

**CORS**
- `cowboy_rest` with custom `content_types_provided` returning permissive `Access-Control-Allow-Origin: *` + credentials headers — credentialed wildcard.

## Tree-sitter seeds (erlang, Cowboy-focused)

```scheme
; cowboy_req:<fn>(...)
(call
  expr: (remote expr1: (atom) @mod expr2: (atom) @fn)
  (#eq? @mod "cowboy_req")
  (#match? @fn "^(binding|parse_qs|read_body|header|headers|method|path|parse_cookies|reply|stream_reply|stream_body)$"))

; binary_to_term / list_to_atom / apply — Erlang-level primitives
(call
  expr: (remote expr1: (atom) @mod expr2: (atom) @fn)
  (#eq? @mod "erlang")
  (#match? @fn "^(binary_to_term|list_to_atom|binary_to_atom|apply|spawn|spawn_link|send)$"))
```
