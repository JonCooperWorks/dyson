Starting points for Ruby on Rails — not exhaustive. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`params[...]` (merges query string + form + JSON body), `request.headers`, `request.cookies`, `cookies.signed[...]` (signed but client-visible), `session[...]` (server-signed but readable if signing key leaks), URL route params, multipart `params[:upload]`.

## Sinks

**SQL injection (ActiveRecord escape hatches)**
- `User.where("name = '#{params[:name]}'")` — string interpolation in `where`.  `where(name: params[:name])` is safe.
- `User.find_by_sql("... #{user}")`, `connection.execute("... #{user}")` — raw SQL with interpolation.
- `.order(params[:sort])`, `.group(params[:g])`, `.select(params[:fields])` — column names unsanitized → SQLi.
- `.joins("... #{user}")` — string JOIN clauses are raw SQL.

**Mass assignment**
- `User.update(params)` / `User.new(params)` — without strong parameters (`params.require(:user).permit(:name, :email)`), attacker sets arbitrary attributes including `admin=true`.
- `params.permit!` — explicitly unsafe; attacker can assign anything.

**Deserialization / session**
- `Marshal.load(cookies[:session])` (default pre-4.1) — RCE on secret leak.  Modern Rails uses `MessageEncryptor` with AES-GCM but signing-key leak still enables forgery.
- `YAML.load(params[:data])` on pre-Psych-3 — RCE.  `YAML.safe_load` is the safe alternative.
- `ActiveSupport::MessageVerifier.verify` with `digest: 'MD5'` — collision attacks.

**Command execution**
- `` system(`cmd #{params[:x]}`) ``, `exec("cmd #{user}")`, `%x{...}` in a controller → RCE.
- `Open3.popen3("cmd #{user}")` — same.

**Eval / dynamic code**
- `eval(params[:code])` — direct RCE.
- `render inline: params[:template]` — SSTI via ERB.
- `constantize` on user string: `params[:class].constantize.find(id)` — arbitrary class resolution; with `.new`, RCE.

**Redirect / SSRF**
- `redirect_to params[:url]` — open redirect unless allowlisted.  `redirect_to url, allow_other_host: false` (Rails 7+).
- `Net::HTTP.get(URI(params[:url]))` — SSRF; no host allowlist.

**File / path**
- `send_file params[:path]` / `render file: params[:path]` — traversal.
- `File.read(params[:path])` in a controller — same.
- `params[:upload].original_filename` — attacker-controlled; `File.open(Rails.root.join("uploads", upload.original_filename))` without `File.basename` is traversal.

**XSS / templates**
- ERB: `<%= raw user %>`, `<%= user.html_safe %>` — XSS.
- HAML / Slim: `!= user`, `== user` — same bypass; verify the template engine's escape rules per block.
- `content_tag(:div, user, ...)` — `user` is auto-escaped; `content_tag(:div, user.html_safe)` is not.

**Authentication / authorization**
- `skip_before_action :authenticate_user!` on a state-changing controller — finding unless an alternative auth check is present in the action body.
- `skip_before_action :verify_authenticity_token` — CSRF disabled; finding unless the action is API-only with an alternative token check.
- `before_action :set_resource` that uses `params[:id]` without a scoped `current_user.resources.find(params[:id])` — IDOR.

**Secrets in source**
- `config/secrets.yml`, `config/master.key`, `config/credentials.yml.enc` — `.key` file committed = signing-key leak → session forgery, Marshal RCE, MessageVerifier forgery.  Check `.gitignore`; flag if committed.
- `config/database.yml` with plaintext production passwords.

**ActiveStorage / uploads**
- `params[:upload].read` piped into a URL-based service (S3, etc.) without MIME / size validation — no finding per se, but combined with serving user uploads as HTML you get stored XSS.

## Tree-sitter seeds (ruby, Rails-focused)

```scheme
; .where / .find_by_sql / .order with a string argument (worth inspection)
(call
  method: (identifier) @m
  (#match? @m "^(where|find_by_sql|order|group|joins|select|having)$")
  arguments: (argument_list (string))) @maybe-sqli

; render inline: / render file:
(call
  method: (identifier) @m
  (#eq? @m "render"))

; constantize / send / public_send — reflection
(call method: (identifier) @m
  (#match? @m "^(constantize|send|public_send|eval|instance_eval|class_eval)$"))

; skip_before_action / skip_forgery_protection — auth bypass markers
(call method: (identifier) @m
  (#match? @m "^(skip_before_action|skip_forgery_protection)$"))
```
