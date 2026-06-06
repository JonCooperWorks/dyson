Starting points for Symfony (PHP) — not exhaustive. Strongest PHP framework; a lot of security lives in its config files — wrong YAML → vulnerable app. Novel sinks outside this list are still in scope.

## Sources (attacker-controlled)
`$request->query->get('k')`, `$request->request->get('k')` (POST), `$request->attributes->get('k')` (route params), `$request->headers->get('H')`, `$request->cookies->get('c')`, `$request->files->get('f')`, `$request->getContent()` (raw body).

Symfony Forms + Validator binds request data into a typed `$form->getData()`.  Used correctly, this closes mass assignment.  `$form->setData($user)` before `$form->handleRequest($request)` ensures only mapped fields are writable.

## Sinks

**SQL (Doctrine + raw)**
- DQL: `$em->createQuery("SELECT u FROM App\\Entity\\User u WHERE u.name = '$name'")` — interpolation: DQL injection.  Use `:name` parameter with `->setParameter('name', $name)`.
- Native SQL: `$em->getConnection()->executeQuery("... $user")` — SQLi; use `?` placeholders + `$params = [$user]`.
- QueryBuilder: `->where("name = '$user'")` — SQLi; `->andWhere('name = :n')->setParameter('n', $user)` is safe.

**Expression Language injection (Symfony-specific)**
- `ExpressionLanguage::evaluate($userExpression)` — arbitrary PHP via the language.  Any code path evaluating a user string in Expression Language is RCE: `constant('DIRECTORY_SEPARATOR')` works, and Symfony's constant gadget lets you invoke `system()` / `exec()` / `create_function()` depending on version.
- `@Security("is_granted('ROLE_ADMIN') or " . $userExpr)` — concat into a security expression.
- Twig `{{ attribute(_self, userAttr) }}` — dynamic attribute access (reflection-ish).

**Template injection (Twig)**
- `{{ user|raw }}` — XSS.
- `{% autoescape false %}` / `autoescape: false` in config — entire templates unescaped.
- `{% set t = user %}{% set x = t|e('html') %}` — safe.  `{% set t = user|striptags|raw %}` is NOT safe; `raw` wins.
- Rendering user-provided template source: `$twig->createTemplate($userSource)->render()` — SSTI.

**Deserialization**
- `unserialize($request->getContent())` — PHP unserialize RCE via POP chains.
- Symfony Serializer (`XmlEncoder`, `YamlEncoder`) on untrusted XML — XXE if `DOMDocument` isn't configured with `LIBXML_NONET | LIBXML_DTDLOAD` disabled.
- `YamlEncoder` with the symfony/yaml default — `Yaml::PARSE_CUSTOM_TAGS` flag must NOT be set on untrusted input.

**Command execution**
- `Process::fromShellCommandline("cmd $user")` — shell; RCE.  `new Process([$bin, $user])` avoids shell but `$bin` user-controlled = RCE.
- `exec($user)`, `shell_exec`, `system`, backticks — standard PHP sinks.

**File / path**
- `new File($request->files->get('f')->getClientOriginalName())` — attacker-supplied filename.
- `$request->files->get('upload')->move($dir, $filename)` — `$filename` from user is traversal without basename + allowlist.
- `$this->render('path/' . $user . '.html.twig')` — template path injection; lets attacker select unintended templates.
- `BinaryFileResponse($userPath)` — direct file disclosure.

**Redirect / SSRF**
- `$this->redirect($userUrl)` / `new RedirectResponse($userUrl)` — open redirect.
- HttpClient: `$client->request('GET', $userUrl)` — SSRF; no default host allowlist.

**Security YAML (config-level)**
- `security.yaml` `firewalls: { main: { pattern: ^/, anonymous: true } }` — anonymous firewall for everything is the default dev config; check prod config differs.
- `access_control: []` empty or permissive — no role-based gates.
- Missing CSRF token on a form: every `FormType` using `'csrf_protection' => false` opts out; check state-changing forms aren't.
- `providers.chain_provider` with user-controlled provider selection — auth bypass risk.

**Voters / authorization**
- Voters returning `VoterInterface::ACCESS_GRANTED` unconditionally on an attribute.  `if ($this->isGranted('EDIT', $post))` without a voter implementing EDIT defaults to DENY — fine.  But a voter that returns GRANTED when `$subject === null` and the caller passes `null` is a bypass.

**Session / CSRF**
- `framework.session.cookie_secure: false` + session cookie used for auth over HTTP.
- `framework.csrf_protection: false` — CSRF protection globally off.
- Custom `CsrfTokenManager` with `SessionTokenStorage` — fine.  With an attacker-readable store (shared cache) — token leak.

## Tree-sitter seeds (php, Symfony-focused)

```scheme
; $request->get* family
(member_call_expression
  object: (variable_name (name) @o)
  name: (name) @m
  (#eq? @o "request")
  (#match? @m "^(get|getContent|query|request|attributes|headers|cookies|files)$"))

; ExpressionLanguage::evaluate / $twig->createTemplate / unserialize
(scoped_call_expression
  scope: (name) @c
  name: (name) @m
  (#match? @c "^(ExpressionLanguage|Yaml|Process)$")
  (#match? @m "^(evaluate|parse|fromShellCommandline)$"))

(function_call_expression
  function: (name) @f
  (#match? @f "^(unserialize|eval|exec|shell_exec|system|passthru)$"))
```
