# Security Review: Grafana Dashboards Service

No exploitable findings meet the Finding Gate criteria within this codebase scope.

All database queries use parameterized placeholders (`?`). No `exec.Command`, `eval`, or unsafe deserialization primitives exist. No hardcoded secrets or credentials are present in source.

The codebase follows Grafana's service-layer pattern: authorization is handled at the HTTP handler layer via middleware, and the dashboard service calls out to `guardian` for certain operations.

## Checked and Cleared

- `database/database.go:104` — `GetFolderByTitle` uses ORM `.Where()` with bound struct fields (`Dashboard{OrgId: orgID, Title: title}`), not string interpolation.
- `database/database.go:121` — `GetFolderByID` same ORM pattern, int64 ID parameterized.
- `database/database.go:145` — `GetFolderByUID` same ORM pattern, UID bound via struct field.
- `database/database.go:320` — `fmt.Sprintf` injects only `BooleanStr(true)` (constant from dialect), not user input.
- `database/database.go:337` — `fmt.Sprintf` injects only `BooleanStr(false)`, user `OrgID` passed as `?` parameter.
- `database/database.go:488-595` — `saveDashboard` — all queries use ORM `.Where()` or `sess.Exec("... ?", param)`. Version check provides TOCTOU protection.
- `database/database.go:924` — `GetDashboard` — ORM `.Get()` with struct field binding (slug, org_id, id, uid).
- `database/database.go:948` — `GetDashboardUIDById` — parameterized `WHERE Id=?`.
- `database/database.go:963` — `GetDashboards` — uses `sess.In()` for IN-clause (parameterized).
- `database/database.go:986` — `FindDashboards` — user `Title` passed via `searchstore.TitleFilter` to `Builder.ToSQL()` returning `(sql, params)`, executed as `sess.SQL(sql, params...)`. Parameterized path.
- `database/database.go:1068` — `GetDashboardTags` — parameterized `WHERE dashboard.org_id=?`.
- `database/acl.go:16` — `GetDashboardACLInfoList` — raw SQL uses `?` parameters for org_id and dashboard_id (line 84).
- `database/acl.go:99` — `HasEditPermissionInFolders` — uses `db.NewSqlBuilder` with `WriteDashboardPermissionFilter` (internal builder).
- `database/acl.go:127` — `HasAdminPermissionInDashboardsOrFolders` — same builder pattern.
- `database/acl.go:153` — `DeleteACLByUser` — parameterized `DELETE WHERE user_id = ?`.
- `database/database.go:762` — `deleteDashboard` — all sub-queries use `?` placeholders.
- `database/database.go:751` — `GetDashboardsByPluginID` — `sess.Where()` with bound params.
- `service/dashboard_service.go:78` — `BuildSaveDashboardCommand` — uses `guardian.New()` for auth checks on folder changes and save/create (lines 123-124, 143-145, 151-152).
- `service/dashboard_service.go:213` — `SaveProvisionedDashboard` — delegates through `BuildSaveDashboardCommand` → Guardian auth.
- `service/dashboard_service.go:303` — `SaveDashboard` — delegates through `BuildSaveDashboardCommand` → Guardian auth.
- `service/dashboard_service.go:353` — `DeleteDashboard` / `deleteDashboard` (line 409) — checks provisioned status before deletion.
- `service/dashboard_service.go:452` — `UnprovisionDashboard` — no internal auth check. Per Finding Gate #1, only reachable from trusted internal callers (HTTP handlers with middleware). Auth is expected at the handler layer.
- `service/dashboard_service.go:178` — `UpdateDashboardACL` — no internal auth check. Same handler-layer auth model.
- `service/dashboard_service.go:361` — `MakeUserAdmin` — no internal auth check. Same handler-layer auth model.
- `accesscontrol.go:42` — `NewFolderNameScopeResolver` — validates prefix before lookup, returns `ErrInvalidScope` on mismatch.
- `accesscontrol.go:61` — `NewFolderIDScopeResolver` — validates prefix and parses scope ID, returns `ErrInvalidScope` on mismatch.
- `accesscontrol.go:88` — `NewDashboardIDScopeResolver` — validates prefix, parses scope ID.
- `accesscontrol.go:111` — `NewDashboardUIDScopeResolver` — validates prefix, parses scope UID.

## Dependencies

No `go.mod` or `go.sum` found in the scanned directory. Dependency review could not run. If this is part of the full Grafana monorepo, `go.mod` should be scanned at the repository root.

## Remediation Summary

No remediation required for this scope.