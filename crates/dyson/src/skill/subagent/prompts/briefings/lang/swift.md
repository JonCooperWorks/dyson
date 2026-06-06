Starting points for Swift — not exhaustive. Server-side Swift (Vapor / Hummingbird) overlaps the usual web attack surface; iOS clients have platform-specific sinks too. Novel sinks outside this list are still in scope.

## Sinks

**Command execution**
- `Process()` + `.launchPath = user` — RCE when `user` is attacker-controlled.
- `Process().arguments = ["-c", user]` with a shell binary → shell RCE.
- `popen(user, "r")` (via C interop) — shell-expanded.

**Unsafe / FFI**
- `UnsafeMutablePointer<T>.init(bitPattern:)` / `UnsafeRawPointer` with untrusted integers — arbitrary memory.
- `withMemoryRebound`, `unsafeBitCast<T, U>(value, to:)` on network bytes — type punning; same UB as Rust `transmute`.
- C interop: any `@_silgen_name` FFI into `memcpy` / `strcpy` with user sizes.

**Reflection / dynamic dispatch**
- `Mirror(reflecting: obj)` is read-only — low risk.
- `NSClassFromString(user)` → `NSObject.Type` → `.init()` → `obj.perform(NSSelectorFromString(user))` is the Obj-C bridge RCE primitive.  Valid on macOS/iOS, not on Linux server-side Swift.
- `NSSelectorFromString(user)` + `perform(_:)` — arbitrary selector invocation.

**Deserialization**
- `JSONDecoder().decode(SomeType.self, from: bytes)` — safe IF `SomeType` is a closed union.  Decoding into `Codable` protocols with unknown concrete types via user-controlled discriminator = polymorphic RCE (rare; requires custom `init(from:)`).
- `NSKeyedUnarchiver.unarchiveTopLevelObjectWithData(_:)` without a class allowlist — Objective-C unarchive RCE.  Use `unarchivedObject(ofClass:from:)` with an explicit class.
- `PropertyListSerialization.propertyList(from:options:format:)` on untrusted plists — XXE-equivalent metadata attacks.

**SQL**
- Vapor `Fluent` ORM — safe via typed query builders.  Raw SQL: `database.raw("... \(user)")` interpolates; use `.bind(user)`.
- `SQLite.swift` `db.prepare("... \(user)")` — interpolation is SQLi.  Use `?` placeholders.

**Path / file**
- `FileManager.default.contents(atPath: user)`, `try String(contentsOfFile: user)` — traversal unless path is validated.
- `URL(fileURLWithPath: user)` + `FileManager.default.removeItem` — attacker-chosen delete.

**URL / SSRF**
- `URLSession.shared.dataTask(with: url)` where `url` is user-constructed — no host allowlist.
- `UIApplication.shared.open(url)` (iOS) with user URL — can launch `tel:`, `sms:`, custom URL schemes that trigger sensitive actions.

**Crypto**
- `arc4random_uniform` is fine on Apple platforms.  `Swift.Random` / `Int.random(in:)` uses a system CSPRNG on Apple — check on Linux (swift-corelibs-foundation may use `/dev/urandom` which is also fine).
- `Insecure.MD5` / `Insecure.SHA1` (CryptoKit) — they're prefixed "Insecure" for a reason; not for password hashing.
- `==` on `Data` / `HMAC<H>.MAC` — timing-unsafe.  Use CryptoKit's `HashedAuthenticationCode` comparison helpers.

**iOS-specific (client-side) that matter server-side too**
- `WebView`'s `evaluateJavaScript(user_js)` — if user-provided, RCE in the web context.
- `UIPasteboard.general.string = secret` + universal clipboard — cross-device leak.
- Insecure `URLSession` config: `URLSessionConfiguration.default` with `.urlCredentialStorage` sharing across sessions.

## Tree-sitter seeds (swift)

```scheme
; Process, URLSession, JSONDecoder/NSKeyedUnarchiver
(call_expression
  (identifier) @ty
  (#match? @ty "^(Process|URLSession|JSONDecoder|NSKeyedUnarchiver|PropertyListSerialization)$"))

; NSClassFromString / NSSelectorFromString
(call_expression
  (identifier) @f
  (#match? @f "^(NSClassFromString|NSSelectorFromString)$"))
```
