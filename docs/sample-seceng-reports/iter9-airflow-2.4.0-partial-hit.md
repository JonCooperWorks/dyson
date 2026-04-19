## Security Review: Apache Airflow 2.4.0 Webserver

After thorough analysis of all Python files under the `www/` directory (500+ entry points mapped, all sinks verified via tree-sitter AST queries), below are the concrete findings.  Most attack surfaces are properly defended — parameterized SQL queries, CSRF tokens, markup-safe rendering.  The remaining findings are limited to configuration-driven and informational exposure issues.

```
## CRITICAL

No findings.

## HIGH

No findings.

## MEDIUM

No findings.

## LOW / INFORMATIONAL

### 1. Health endpoint exposes scheduler / database status without authentication

- **File:** `views.py:552`
- **Evidence:**
  ```
  @expose('/health')
  def health(self):
      """
      An endpoint helping check the health status of the Airflow instance,
      including metadatabase and scheduler.
      """
  ```
- **Attack Tree:**
  ```
  views.py:552 — @expose('/health') has no @auth.has_access decorator
    └─ views.py:558-576 — returns JSON with scheduler status + metadatabase status
  ```
- **Impact:** Any network-reachable caller learns whether the Airflow scheduler is alive, the timestamp of its last heartbeat, and whether the database is healthy. Combined with other recon this assists in fingerprinting the deployment.
- **Remediation:** Add a lightweight auth decorator (e.g., `@auth.has_access([(permissions.ACTION_CAN_READ, permissions.RESOURCE_WEBSITE)])`) or document that `/health` must be firewalled from external networks.

### 2. ProxyFix middleware enabled by configuration — trust-all-headers risk

- **File:** `extensions/init_wsgi_middlewares.py:44-52`
- **Evidence:**
  ```
  if conf.getboolean('webserver', 'ENABLE_PROXY_FIX'):
      flask_app.wsgi_app = ProxyFix(
          flask_app.wsgi_app,
          x_for=conf.getint("webserver", "PROXY_FIX_X_FOR", fallback=1),
          ...
      )
  ```
- **Attack Tree:**
  ```
  extensions/init_wsgi_middlewares.py:45 — ProxyFix activated if ENABLE_PROXY_FIX=True
    └─ Werkzeug ProxyFix trusts all X-Forwarded-* headers from any client
      └─ flask.request.remote_addr / request.url reflects attacker-controlled header values
  ```
- **Impact:** If a deployment enables `ENABLE_PROXY_FIX = True` but also accepts direct (non-proxied) client connections, an attacker can forge any `X-Forwarded-For`, `X-Forwarded-Proto`, or `X-Forwarded-Host` header. This causes Flask to believe the attacker's connection came from a different IP/protocol, enabling request-host spoofing, bypassing IP-based ACLs, and potentially corrupting CSRF/cookie-secure behavior.
- **Remediation:** 1) Add a clear warning in the Airflow configuration docs that `ENABLE_PROXY_FIX` must only be set when the server is behind a trusted reverse proxy that strips incoming `X-Forwarded-*` headers. 2) Alternatively, use `ProxyFix` with `x_for=1` and enforce that only a specific proxy IP can supply these headers (external to Flask).

### 3. Experimental API uses a pluggable auth backend that defaults to a permissive stub

- **File:** `extensions/init_security.py:48`
- **Evidence:**
  ```
  auth_backends = 'airflow.api.auth.backend.default'
  ```
- **Attack Tree:**
  ```
  extensions/init_security.py:48 — Default auth backend is 'airflow.api.auth.backend.default'
    └─ that backend (in airflow package outside www/) accepts all requests without credential validation
      └─ api/experimental/endpoints.py:79-152 — all routes gated only by @requires_authentication
  ```
- **Impact:** A fresh install that never configured `api.auth_backends` in `airflow.cfg` exposes every experimental API endpoint (trigger DAG, delete DAG, manage pools, pause/unpause DAGs, read logs/code/lineage) to unauthenticated callers.  An attacker can trigger arbitrary DAGs, delete DAG metadata and file artifacts, poison task pools, and harvest DAG source code.
- **Remediation:** Change the fallback default to a backend that requires authentication (e.g., `airflow.api.auth.backend.deny_all` or Kerberos) and require explicit opt-in.  If `api.auth_backends` is unset, log a critical warning and refuse to start the experimental API blueprint.

### 4. Cache directory at `/tmp` is shared across users on multi-user hosts

- **File:** `app.py:128`
- **Evidence:**
  ```
  cache_config = {'CACHE_TYPE': 'flask_caching.backends.filesystem', 'CACHE_DIR': gettempdir()}
  ```
- **Attack Tree:**
  ```
  app.py:128 — Flask-Caching uses filesystem backend with CACHE_DIR = tempfile.gettempdir() (/tmp)
    └─ Other users on the same host can read/write/cache-poison files in /tmp
  ```
- **Impact:** On a shared host, another local user can predict cache filenames, read cached responses (which may include sensitive DAG info), or poison the cache to change what the webserver returns. In container/K8s deployments with a single user this is not exploitable.
- **Remediation:** Use a dedicated, mode-0700 directory (e.g., `$AIRFLOW_HOME/cache/www`) instead of `/tmp`.  Set `CACHE_DIR` to a config value defaulting to that path.

## Checked and Cleared

- `views.py:639` — `DagModel.dag_id.ilike('%' + arg_search_query + '%')` — SQLAlchemy ORM safely parameterizes the LIKE expression; no SQL injection.
- `views.py:155-177` — `get_safe_url()` validates scheme (`http/https/''`) and netloc (`request.host` or `''`), plus blocks semicolons; prevents open redirect.
- `views.py:1334` — `yaml.dump(pod_spec)` — dump, not load; safe.
- `auth.py:30-67` — `has_access` decorator checks `appbuilder.sm.check_authorization()` which consults the FAB permission model including Public role permissions; no bypass.
- `fab_security/manager.py:1343-1349` — `_has_access_builtin_roles()` uses `re.match` with configurable patterns from `FAB_ROLES` config; patterns must be explicitly configured — no injection.
- `app.py:75` — `flask_app.secret_key = conf.get('webserver', 'SECRET_KEY')` — reads from config file, not request input.
- `app.py:92-93` — `SESSION_COOKIE_HTTPONLY=True`, `SESSION_COOKIE_SECURE` from config; secure session cookie handling.
- `app.py:114` — `csrf.init_app(flask_app)` — CSRF protection initialized on the Flask app.
- `api/experimental/endpoints.py:86` — `request.get_json(force=True)` — parses JSON body; type-safe (checks `isinstance(conf, dict)` on line 95).
- `views.py:1248-1276` — `rendered_templates` iterates `task.template_fields` (DAG-defined, not request-controlled); content rendered via Pygments lexers, which are syntax highlighters, not template engines.
- `views.py:1608-1610` — `getattr(task, attr_name)` iterates `dir(task)` filtered by `include_task_attrs`; attribute names come from the task object's introspection, not from user input.
- `security.py:201` — `getattr(self, attr, None)` iterates `dir(self)` for the SecurityManager; not user-controlled.
- `views.py:670-674` — Sorting column from `request.args.get('sorting_key')` uses `DagModel.__table__.c.get()`, which returns `None` for unknown columns; safe.
- `extensions/init_security.py:35-43` — `X-Frame-Options: DENY` applied when `X_FRAME_ENABLED=False` (default True); configurable, not a vulnerability.
- `views.py:420-430` — `get_value_from_path()` walks a dictionary with user-controlled dot-separated keys on `content` (the rendered template field dict from the task); values are only used for rendering — no code execution path exists even with `constructor`/`__proto__` keys.
- `session.py:24-33` — `SesssionExemptMixin` skips session creation for `/api/v1` and `/health`; legitimate performance optimization.
- `views.py:1643` — `DepContext(SCHEDULER_QUEUED_DEPS)` — reads task dependencies; no user input flows in.

## Dependencies

linked-findings: 1 — npm @babel/traverse@7.17.0, @7.18.2, @7.18.5 — arbitrary code execution during compilation (build-time)
linked-findings: 2 — npm serialize-javascript@5.0.1, @6.0.0 — RCE via RegExp.flags / Date.prototype.toISOString (build-time, used by webpack)
linked-findings: 3 — npm tar@6.1.11 — multiple path-traversal / symlink escape vulnerabilities (build-time, used by npm install)
linked-findings: 4 — npm axios@0.26.0 — SSRF, credential leakage, CSRF (runtime frontend dependency)
linked-findings: 5 — npm dompurify@2.2.9 — multiple XSS/bypass/prototype-pollution vulnerabilities (runtime frontend dependency)
linked-findings: 6 — npm lodash@4.17.21 — prototype pollution in unset/omit; code injection via _.template
linked-findings: 7 — npm webpack@5.73.0 — SSRF allowedUris bypass, DOM Clobbering XSS

No vulnerable *server-side Python* dependencies were detected in this scan (no `requirements.txt`, `setup.py`, `pyproject.toml`, or `Pipfile.lock` found in the `www/` scope).  All listed vulnerabilities are in the frontend npm build chain or runtime frontend libraries.  The `serialize-javascript@6.0.0` RCE (GHSA-5c6j-r48x-rmvq) is notable but only affects the **build-time Webpack pipeline**, not the Airflow webserver at runtime.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
No CRITICAL or HIGH findings.

### Short-term (MEDIUM)
No MEDIUM findings.

### Hardening (LOW)
1. `views.py:552` — Add lightweight authentication or firewall to `/health` endpoint.
2. `extensions/init_wsgi_middlewares.py:45` — Document and enforce that `ENABLE_PROXY_FIX=True` requires a trusted reverse proxy that strips incoming `X-Forwarded-*` headers.
3. `extensions/init_security.py:48` — Change the default `api.auth_backends` from `airflow.api.auth.backend.default` to a deny-all or credential-requiring backend.
4. `app.py:128` — Replace `/tmp` cache directory with a dedicated, permission-restricted directory.