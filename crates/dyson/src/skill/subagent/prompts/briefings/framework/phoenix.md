Starting points for Phoenix (Elixir) — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`conn.params`, `conn.body_params`, `conn.query_params`, `conn.path_params`, `conn.req_headers`, `conn.cookies`, `params` in a controller action, socket `params` / `message` payloads in a LiveView / Channel.

## Sinks

**SQL injection (Ecto)**
- `Ecto.Adapters.SQL.query!(Repo, "SELECT ... #{user}", [])` — interpolated string.  Use `query!(Repo, "SELECT ... $1", [user])`.
- `from u in User, where: fragment("name = #{^user}")` — fragment interpolation; use `fragment("name = ?", ^user)` pin form with `?` placeholders.
- `Ecto.Query.API.fragment/1` with `#{}` interpolation.
- Raw `Ecto.Adapters.SQL.query/3` with user-constructed SQL string.

**Deserialization**
- `:erlang.binary_to_term(conn.assigns[:blob])` — RCE without `[:safe]` option.  Always `binary_to_term(bytes, [:safe])`.
- Phoenix sessions with `:cookie` store + pickle-like custom serializer (rare; check).
- `Plug.Session` configured with an unsafe serializer — default `Plug.Crypto.KeyGenerator` + `:erlang.term_to_binary` cycle is safe AS LONG AS the signing / encryption keys are not leaked.

**Command execution**
- `System.cmd(params["bin"], args)` — `params["bin"]` user-controlled = RCE.
- `System.cmd("sh", ["-c", params["cmd"]])` — shell RCE.
- `:os.cmd(params["cmd"] |> String.to_charlist())` — shell-expanded.

**Dynamic atom / dispatch**
- `String.to_atom(params["key"])` — atom exhaustion DoS (atoms aren't GC'd; default cap ~1M).  Use `String.to_existing_atom/1`.
- `apply(MyModule, String.to_atom(params["fn"]), args)` — attacker picks the function to invoke.
- `Module.concat(params["mod"])` then `.function(...)` — arbitrary module resolution.

**Template / XSS**
- `Phoenix.HTML.raw/1` — bypasses auto-escape.  `raw(user_html)` in a template = XSS.
- `~H"<div>{@user}</div>"` (HEEx) — escaped by default.  `~H"<div>{raw(@user)}</div>"` — bypass.
- Old `~E` (EEx) templates: `<%= user %>` vs `<%= raw(user) %>` — same distinction.
- Custom components using `render_slot/1` that interpolate slot content without escaping.

**Redirect / SSRF**
- `redirect(conn, external: params["url"])` — open redirect.  `redirect(conn, to: "/path")` with a controlled path is safer.
- `HTTPoison.get(params["url"])` / `Finch.request(...)` — SSRF.  No host allowlist.

**File / path**
- `File.read!(params["path"])` — traversal.
- `Plug.Static.init(at: "/", from: params["root"])` at configure time — fine; but `send_download(conn, {:file, params["path"]})` at request time is traversal.

**Authentication / authorization**
- Missing `pipe_through [:browser, :require_authenticated_user]` on a route scope.
- `plug :accepts, ["json"]` without `plug :authenticate_api_user`.
- `conn.assigns[:current_user] || something` — if `:current_user` is `nil`, `|| something` bypasses auth.

**LiveView-specific**
- `handle_event("save", params, socket)` — `params` is attacker-controlled.  Writing `params["user_id"]` directly into `socket.assigns` or passing to `Repo.get` without checking `socket.assigns.current_user` against `user_id` = IDOR.
- LiveView `push_event/3` with user-interpolated data — XSS in the client-rendered event payload.
- `mount/3` must check auth: `mount(_params, _session, socket) do ... end` without checking `socket.assigns.current_user` = unauth access.

**CSRF**
- `pipeline :api do plug :accepts, ["json"] end` — no CSRF plug.  Fine for pure-token APIs; finding if session cookies are used.
- `protect_from_forgery` disabled in the `:browser` pipeline.

**Crypto / secrets**
- `Application.get_env(:my_app, :secret_key_base, "dev-secret")` — fallback literal is a committed secret.
- `config/config.exs` / `config/prod.secret.exs` with plaintext secrets → finding.
- `:crypto.hash(:md5, pass)` for password hashing — use `Argon2` / `Bcrypt` / `Pbkdf2`.

## Tree-sitter seeds (elixir, Phoenix-focused)

```scheme
; Ecto fragment / raw query
(call target: (dot left: (alias) @m right: (identifier) @fn)
  (#match? @m "^(Repo|Ecto.Adapters.SQL)$")
  (#match? @fn "^(query|query!|select_all|insert_all)$"))

; binary_to_term / to_atom
(call target: (dot left: (alias) @m right: (identifier) @fn)
  (#match? @m "^(:erlang|String|Module)$")
  (#match? @fn "^(binary_to_term|to_atom|concat|safe_concat)$"))

; redirect / raw
(call target: (identifier) @f
  (#match? @f "^(redirect|raw|render|send_file|send_download)$"))
```
