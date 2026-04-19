# Security Review: Django ORM Functions

## Scope

`django/db/models/functions/` - Django 3.2.14 query builder functions (text, datetime, math, comparison, window, mixins). These are expression-tree nodes that generate SQL fragments, not application-level code.

## Summary

No exploitable vulnerabilities found. All SQL generation uses Django's ORM parameterization system (`%(expressions)s` template placeholders compiled through `compiler.compile()`), identifier quoting (`connection.ops.quote_name()`), and allowlist-style regex validation on dynamic values (`lookup_name`, `kind`, `collation`).

## Checked and Cleared

- `text.py:13` — `template='SHA2(%%(expressions)s, %s)' % self.function[3:]` — `self.function` is a hardcoded class attribute (`'SHA224'`, `'SHA256'`, etc.), not attacker-controlled.
- `text.py:76` — `template = '%(expressions)s %(function)s %(collation)s'` with `collation` validated by `^[\w\-]+$` regex and passed through `connection.ops.quote_name()` — safe on all four backends (PostgreSQL double-quotes, MySQL backticks, Oracle double-quotes with truncation, SQLite double-quotes).
- `text.py:223-227` — `Reverse.as_oracle` template with `%(expressions)s` in `SUBSTR`, `LENGTH`, `GROUP BY` — `%(expressions)s` is the Django ORM placeholder for `compiler.compile(self.lhs)`, not raw user input.
- `datetime.py:44-50` — `Extract.as_sql` validates `lookup_name` against `extract_trunc_lookup_pattern` (regex `[\w\-_()]+`), then delegates to `connection.ops.datetime_extract_sql`/`date_extract_sql`/`time_extract_sql` which use `%s` formatting for `field_name` (ORM-compiled SQL) and `tzname` (from system settings or explicit `tzinfo`, not user request data).
- `datetime.py:197-212` — `TruncBase.as_sql` same validation pattern for `kind` via `extract_trunc_lookup_pattern`.
- `datetime.py:67-88` — `Extract.resolve_expression` validates `lhs.output_field` is a date/time/duration field, rejects time components on `DateField` — type safety guard, not a vulnerability path.
- `datetime.py:215-242` — `TruncBase.resolve_expression` validates `output_field` types and prevents truncating `DateField` to `DateTimeField` — type safety.
- `math.py:28` — `FixDurationInputMixin.as_mysql`: `sql = 'CAST(%s AS SIGNED)' % sql` — `sql` is the return value of `super().as_sql()`, i.e., ORM-compiled SQL fragment with parameterized values, not raw user input.
- `math.py:80` — `Degrees.as_oracle` template uses `% math.pi` — `math.pi` is a Python constant.
- `math.py:139` — `Radians.as_oracle` template uses `% math.pi` — same.
- `comparison.py:11` — `Cast.template = '%(function)s(%(expressions)s AS %(db_type)s)'` — `db_type` comes from `self.output_field.cast_db_type(connection)`, a developer-defined field type.
- `text.py:174-176` — `MD5` class, `text.py:249-251` — `SHA1` class — these expose SQL `MD5()`/`SHA1()` functions. Using weak hashes is the developer's explicit choice (they call `MD5(expression)` in their query), not a vulnerability in the framework itself.

## Dependencies

No vulnerable Python dependencies found in the review scope. The `dependency_review` subagent flagged `grunt@1.0.1` (npm dev dependency for JS tests) with three advisories — path traversal, arbitrary code execution, race condition — but these are build/test tool vulnerabilities, not in Django's Python runtime. Not relevant to the code under review.

### Remediation Summary

No remediation required.