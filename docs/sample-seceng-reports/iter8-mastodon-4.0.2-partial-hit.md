# Security Review: Mastodon v4.0.2 (lib/ subset)

## MEDIUM

### SSRF via federation WebFinger host-meta template redirect
- **File:** `webfinger.rb:85`
- **Evidence:**
  ```ruby
  link['template'].gsub('{uri}', @uri)
  ```
- **Attack Tree:**
  ```
  ActivityPub::FetchRemoteAccountService (federated account lookup) — attacker controls remote domain
    └─ WebFinger.perform → body_from_webfinger → 404 response triggers fallback
      └─ webfinger.rb:71-74 — body_from_host_meta fetches host-meta XML from attacker domain
        └─ webfinger.rb:85 — url_from_template reads attacker-supplied <Link template=""> attribute
          └─ webfinger.rb:98 — Request.new(:get, url_from_template) — SSRF to attacker-chosen URL
  ```
- **Taint Trace:**
  ```
  taint_trace: lossy — verify every hop with read_file before filing
  index: language=Ruby, files=132, defs=1235, calls=3736, unresolved_callees=7

  Found 2 candidate path(s) from webfinger.rb:74 to webfinger.rb:85:

  Path 1 (depth 2, resolved 3/3 hops):
    webfinger.rb:74 [byte 1740-1814] — fn `body_from_host_meta` — taint root: Error, Webfinger, body_from_webfinger, body_with_limit, code, raise, res, url_from_template
    └─ webfinger.rb:74 [byte 1768-1806] — calls `url_from_template([res+body_with_limit])` → param `str`
      └─ webfinger.rb:85 [byte 2050-2092] — [SINK REACHED] — tainted at sink: link, str
  ```
- **Impact:** A remote Mastodon instance operator (attacker-controlled domain) can redirect a WebFinger lookup to redirect the subsequent WebFinger request to an arbitrary URL via the `<Link template>` attribute in the host-meta XML response. The `Request.Socket.open` private-address check on lines 212–271 validates resolved IPs against `PrivateAddressCheck`, blocking direct private-address SSRF (10.x, 127.0.0.1, 169.254.x.x), but the primitive still allows: (a) redirecting federation lookups to arbitrary external attacker-controlled endpoints that may harvest federation credentials or serve malicious ActivityPub payloads, and (b) DNS-rebinding attacks against the 5-second DNS timeout window. The `@uri` value (e.g. `acct:user@localhost`) is also injected into the template's `{uri}` placeholder, allowing path injection within the attacker-chosen URL.
- **Remediation:** Validate the template URL's host against the host-meta response's origin before following it. Add a provenance check in `url_from_template`:
  ```ruby
  def url_from_template(str)
    link = Nokogiri::XML(str).at_xpath('//xmlns:Link[@rel="lrdd"]')
    return raise Webfinger::Error, "..." unless link.present?

    template_url = URL.from_string(link['template'].gsub('{uri}', @uri))
    # ENSURE: template URL host matches the host-meta response origin
    unless template_url.host.casecasecmp?(@domain) || valid_well_known_url?(template_url)
      raise Webfinger::Error, "Template URL origin mismatch"
    end
    template_url.to_s
  end
  ```

### Stored HTML injection via LinkDetailsExtractor iframe with attacker-controlled src
- **File:** `link_details_extractor.rb:143`
- **Evidence:**
  ```ruby
  content_tag(:iframe, nil, src: player_url, width: width, height: height, allowtransparency: 'true', scrolling: 'no', frameborder: '0')
  ```
- **Attack Tree:**
  ```
  User posts URL → LinkCrawlWorker fetches page → LinkDetailsExtractor.new(url, html, charset)
    └─ opengraph_tag('twitter:player') reads <meta name="twitter:player" content="..."> from crawled HTML
      └─ valid_url_or_nil validates scheme but allows http(s) to any host (line 215)
        └─ player_url returns attacker-controlled URL (line 201)
          └─ html method wraps it in unsandboxed iframe (line 143)
            └─ to_preview_card_attributes[:html] stored in PreviewCard
              └─ served via API to all users viewing the link preview
  ```
- **Impact:** Any user posting a link to an attacker-controlled page causes Mastodon to store an HTML fragment containing `<iframe src="ATTACKER_URL" allowtransparency="true">` in the database. This iframe has no `sandbox` attribute, meaning the loaded page runs with full page permissions in the victim's browser. While loaded from a different origin (so direct DOM access is blocked by SOP), the iframe can: (1) perform CSRF against the embedding page via form submission, (2) capture user interaction patterns, (3) abuse CORS if the Mastodon instance allows it. The URL is validated to be http/https only, blocking `javascript:` and `data:` URI schemes, but the unsandboxed iframe remains a risk vector.
- **Remediation:** Add `sandbox: ''` (or explicit `sandbox: 'allow-scripts'`) to the iframe attributes:
  ```ruby
  player_url.present? ? content_tag(:iframe, nil,
    src: player_url, width: width, height: height,
    allowtransparency: 'true', scrolling: 'no', frameborder: '0',
    sandbox: ''
  ) : nil
  ```

## LOW / INFORMATIONAL

### SQL string interpolation in admin index estimate routine (admin-only, no user input)
- **File:** `importer/base_importer.rb:37`
- **Evidence:**
  ```ruby
  connection.select_one("SELECT reltuples AS estimate FROM pg_class WHERE relname = '#{index.adapter.target.table_name}'")['estimate'].to_i
  ```
- **Impact:** The `table_name` is interpolated into an SQL query string without parameterization. However, `table_name` comes from the ActiveRecord model's schema attribute (`index.adapter.target.table_name`), which is a compile-time constant derived from Rails' table naming convention, not from user input. This is a code smell but not exploitable.
- **Remediation:** Use parameterized query:
  ```ruby
  connection.select_one("SELECT reltuples AS estimate FROM pg_class WHERE relname = ?", index.adapter.target.table_name)
  ```

### Unscoped `send(key)` via ActiveModel serialization interface
- **File:** `admin/metrics/measure/base_measure.rb:54` and `admin/metrics/dimension/base_dimension.rb:39`
- **Evidence:**
  ```ruby
  send(key) if respond_to?(key)
  ```
- **Impact:** `read_attribute_for_serialization` is called by `ActiveModel::Serializers` during JSON serialization. The `key` argument originates from the serializer's attribute introspection, not from user input. While technically a `send` call over an unrestricted key, the attacker has no practical way to control the `key` parameter through the application's request flow — it is determined by the serializer's declared `attributes` list. No finding without a source-to-sink attack path.

### Dependency on outdated Nokogiri with known libxml2 RCE CVEs
- **File:** Gemfile.lock (nokogiri@1.13.9)
- **Impact:** Nokogiri 1.13.9 bundles vulnerable libxml2/libxslt versions affected by GHSA-pxvg-2qj5-37jq (RCE), GHSA-r95h-9x8f-r3f7 (DoS), and GHSA-vvfq-8hwr-qm4m (XXE). These are reachable from `LinkDetailsExtractor` (line 258, `Nokogiri::HTML(@html)`) and the WebFinger parser (`Nokogiri::XML(str)` on line 82). Upgrade to ≥1.19.1.

## Checked and Cleared

- `request.rb:212-271` — SSRF mitigation: custom `Socket.open` validates resolved IPs against `PrivateAddressCheck` before connecting; DNS resolution is capped at 5s timeout; addresses are cached before connection (no race condition on connect).
- `request.rb:59` — `http_client.public_send(@verb, ...)` — `@verb` is set in `initialize` (line 30), not user-controlled.
- `activitypub/activity/create.rb:84` — `Status.create!(@params)` — `@params` is built from `StatusParser` output, not direct user input.
- `activitypub/activity/create.rb:109` — `StatusParser.new(@json, ...)` — `@json` is federation payload, validated by `invalid_origin?` check on line 48.
- `activitypub/linked_data_signature.rb:30` — RSA signature verification via SHA-256 with proper keypair lookup.
- `text_formatter.rb:48` — `html.html_safe` — all entity text is escaped via `h()` on lines 62, 67, 82–83, 93–94, 119, 125.
- `emoji_formatter.rb:59` — `tree.to_html.html_safe` — input is pre-sanitized HTML (must be html_safe to enter, line 16).
- `feed_manager.rb:112-119, 138-145` — SQL queries use parameterized placeholders (`?`) with `oldest_home_score` (integer).
- `feed_manager.rb:21` — `where('users.current_sign_in_at > ?', ...)` — parameterized.
- `search_query_transformer.rb:39` — Elasticsearch clause filter uses symbol keys (`:account_id`), not user strings.
- `admin/metrics/retention.rb:74` — SQL uses parameterized values (`[[nil, @start_at], [nil, @end_at], [nil, @frequency]]`); `@frequency` is whitelist-validated on line 21.
- `admin/system_check/rules_check.rb:7` — `current_user.can?(:manage_rules)` — authorization check present.
- `user_settings_decorator.rb:10-42` — settings update via explicit named keys only, no mass assignment.
- `activitypub/activity/flag.rb:7-8` — `object_uris.filter_map(...).select(&:local?)` — only local accounts/statuses are targetted.
- `activitypub/activity/undo.rb:47` — `@account.follow_requests.find_by(uri: object_uri)` — `object_uri` is federation-derived, validated by origin check.
- `hash_object.rb:7` — `self.class.send(:define_method, ...)` — key names are from hash initialization, constant.
- `scope_parser.rb` — `Oj.load(body, mode: :strict)` — strict mode, no eval.
- `importer/statuses_index_importer.rb:83` — `select('polls.id, polls.status_id')` — fixed column names, not interpolated.
- `activitypub/activity/move.rb:7-15` — Move activity requires `origin_account.uri == object_uri` AND `target_account.also_known_as.include?(origin_account.uri)` — proper origin validation.
- `activitypub/activity/create.rb:48` — `invalid_origin?(object_uri)` — federation origin validation on Create.
- `webfinger.rb:13` — `Oj.load(body, mode: :strict)` — strict JSON parsing, no eval.
- `activitypub/dereferencer.rb:61-67` — `invalid_origin?` checks host matches `permitted_origin`.
- `activitypub/dereferencer.rb:31` — Bearer token from `bear:` URI parsed via `Addressable::URI.parse`, used in outbound Authorization header.
- `vacuum/statuses_vacuum.rb:40` — `execute('ANALYZE statuses')` — fixed string, not interpolated.
- `admin/metrics/dimension/software_versions_dimension.rb:39` — `execute('SELECT VERSION()')` — fixed string.
- `admin/metrics/dimension/space_usage_dimension.rb:18` — `execute('SELECT pg_database_size(current_database())')` — fixed string.

## Dependencies

linked-findings: `app/mailers/user_mailer.rb:23` (devise mailers use actionmailer), `app/models/user.rb` (devise two-factor), `app/models/account.rb` (ActiveRecord), `config/application.rb` (Rack middleware), `config.puma.rb` (Puma 5.6.5), `Gemfile:42` (omniauth-saml ~1.10), `Gemfile:34` (devise-two-factor ~4.0), `Gemfile:33` (devise 4.8.1), `streaming/index.js` (express, ws)

**Critical/High Dependency Vulnerabilities (120 total across 2062 dependencies):**

- **rails@6.1.7** (RubyGems) — SQL injection, XSS, CSP bypass, ReDoS, path traversal across ActionPack, ActiveRecord, ActiveStorage, ActiveSupport. Fixed in ≥6.1.7.1.
- **rack@2.2.4** (RubyGems) — Multipart DoS, LFI in Rack::Static, session restoration after deletion, multipart WAF bypass, stored XSS in Rack::Directory. Fixed in ≥2.2.23.
- **puma@5.6.5** (RubyGems) — HTTP request/response smuggling, header normalization proxy clobbering. Fixed in ≥5.6.9.
- **ruby-saml@1.13.0** (RubyGems) — SAML auth bypass via parser differential, namespace handling, canonicalization bypass. Fixed in ≥1.18.0.
- **devise-two-factor@4.0.2** (RubyGems) — Insufficient default OTP shared secret length. Fixed in 6.0.0.
- **nokogiri@1.13.9** (RubyGems) — libxml2/libxslt RCE, DoS, XXE (GHSA-pxvg-2qj5-37jq). Fixed in ≥1.19.1.
- **sanitize@6.0.0** (RubyGems) — XSS via style/noscript elements. Fixed in ≥6.0.2.
- **doorkeeper@5.6.0** (RubyGems) — Improper authentication. Fixed in ≥5.6.6.
- **devise@4.8.1** (RubyGems) — Confirmable race condition email bypass. Fixed in ≥5.0.3.
- **omniauth@1.9.2 / omniauth-saml@1.10.3** (RubyGems) — CSRF in request phase, signature verification bypass, signature wrapping. Fixed in omniauth 2.0.0 / omniauth-saml 2.2.1.
- **axios@1.1.3** (npm) — SSRF via absolute URL, NO_PROXY bypass, CSRF. Fixed in ≥1.8.2.
- **express@4.18.2** (npm) — XSS via response.redirect, open redirect. Fixed in ≥4.20.0.
- **ws@7.4.6/8.10.0** (npm) — DoS via many HTTP headers. Fixed in ≥7.5.10/8.17.1.
- **faraday@1.9.3** (RubyGems) — SSRF via protocol-relative URL. Fixed in ≥1.10.5.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `webfinger.rb:85` — Validate host-meta template URL origin against the host-meta response's domain before following the redirect.
2. `link_details_extractor.rb:143` — Add `sandbox: ''` attribute to generated iframe elements.
3. `Gemfile` — Bump Rails to ≥6.1.7.9, Rack to ≥2.2.23, Puma to ≥5.6.9, ruby-saml to ≥1.18.0, Sanitize to ≥6.0.2, Doorkeeper to ≥5.6.6, Devise to ≥5.0.3, omniauth-saml to ≥2.2.1.
4. `streaming/yarn.lock` — Upgrade express ≥4.20.0, ws ≥8.17.1.

### Short-term (MEDIUM)
1. `Gemfile` — Upgrade omniauth to ≥2.0.0 (CSRF in request phase), faraday ≥1.10.5 (SSRF via protocol-relative URL).

### Hardening (LOW)
1. `importer/base_importer.rb:37` — Parameterize the SQL query for table name.
2. `admin/metrics/measure/base_measure.rb:54` and `admin/metrics/dimension/base_dimension.rb:39` — Add allowlist for `send(key)` keys in serialization.