Starting points for Sinatra (Ruby) — not exhaustive. Minimal framework = minimal defenses; everything the handler does with `params` is on the handler. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`params` (merged query / form / URL segments), `params[:id]`, `request.body.read`, `request.headers`, `cookies`, `session` (signed via `enable :sessions` with `set :session_secret, ...`).

## Sinks

**SQL (Sequel / ActiveRecord / raw)**
- `DB["SELECT ... '#{params[:name]}'"]` — interpolation in Sequel dataset: SQLi.  Use `DB["SELECT ... ?", params[:name]]`.
- Sequel `DB.fetch("...", params[:x])` is safe; `DB.fetch("... #{params[:x]}")` is not.
- ActiveRecord (if imported): same risks as the Rails sheet — `User.where("name = '#{params[:name]}'")`.

**Command execution**
- `` system("convert #{params[:file]}.png out.png") `` — shell-expanded.
- `%x{...}`, backticks, `exec`, `Kernel.spawn` — all shell routes.

**Eval / dynamic code**
- `eval(params[:code])` — RCE.
- `send(params[:method])` / `public_send(params[:method])` — reflection.
- `Object.const_get(params[:class]).new(...)` — class-from-string.

**Deserialization**
- `Marshal.load(params[:blob])`, `YAML.load(params[:y])` (pre-Ruby 3.1 default) — RCE.
- Session cookies: `enable :sessions` uses Rack's signed cookies.  A leaked `session_secret` lets attackers forge session state.  If `:session_store` is something custom using `Marshal`, forged sessions → RCE.

**Path / file**
- `send_file(params[:path])` — traversal unless anchored to a base directory with realpath check.
- `File.read(params[:path])`, `File.open(params[:path])` — same.
- `File.open(params[:upload][:tempfile])` + `IO.copy_stream(...)` with `params[:upload][:filename]` — attacker filename.

**XSS / templates**
- ERB: `<%= raw(params[:name]) %>`, `<%= params[:name].html_safe %>` — XSS.  `<%= h(params[:name]) %>` / `<%= CGI.escapeHTML(params[:name]) %>` encodes.
- Haml: `!= user` / `= raw(user)` — unescaped.
- Erubis / Slim — similar `raw` / `!` escape-bypass conventions.
- Templates rendered via `erb :index, locals: { user: params[:name] }` — `user` inside the template determines whether it's escaped.

**Redirect / SSRF**
- `redirect params[:url]` — open redirect.  Rack's `Rack::Utils.secure_compare` doesn't help here; need an allowlist check.
- `Net::HTTP.get(URI(params[:url]))` — SSRF.  `open(params[:url])` with `open-uri` honors `http:` / `https:` / sometimes `file:` depending on Ruby version.

**Authentication**
- Sinatra itself has no auth primitive.  Most apps use `rack-protection` (enabled by default for classic apps; DISABLED for modular apps unless `register Sinatra::Protection` or `use Rack::Protection`).  A modular app without `Rack::Protection` is missing CSRF / framing defenses.
- `before` filter checking `session[:user_id]` but not verifying against DB — stale-user problem after the user is banned / deleted.

**Sessions + CSRF**
- `enable :sessions` with the default ephemeral cookie store (rack session) — `session_secret` defaults to something weak in old Sinatra versions; explicit `set :session_secret, ENV['SECRET']` required.
- `use Rack::Protection::AuthenticityToken` — CSRF token middleware; absence on state-changing routes is a finding.

## Tree-sitter seeds (ruby, Sinatra-focused)

```scheme
; Route DSL: get '/x', post '/y', etc.
(call method: (identifier) @m
  (#match? @m "^(get|post|put|patch|delete|options|head)$")
  arguments: (argument_list (string))) @route

; params[:key] access
(element_reference
  object: (identifier) @o
  (#eq? @o "params"))

; redirect / send_file / erb / haml
(call method: (identifier) @m
  (#match? @m "^(redirect|send_file|erb|haml|slim|raw|html_safe)$"))
```
