Starting points for Zig — not exhaustive. No GC, no runtime reflection; memory-safety and allocator discipline dominate. Novel sinks outside this list are still in scope.

## Sinks

**Memory safety**
- `@ptrCast(*T, bytes)` — unchecked cast; with network-derived `bytes`, UB (alignment, lifetime, size).
- `@bitCast(T, value)` on network-derived `value` — same type-punning concerns as Rust `transmute`.  Fine when `T` and `value`'s type have the same size and all bit patterns are valid; dangerous otherwise.
- `allocator.alloc(T, user_len)` where `user_len` isn't validated — allocator OOM or huge allocation (DoS) and downstream OOB if `len` is later mismatched.
- Slice index `slice[user_idx]` — runtime bounds check in Debug/ReleaseSafe, but **undefined in ReleaseFast / ReleaseSmall**.  A security-sensitive binary compiled ReleaseFast has no bounds check; an OOB index is UB.
- `*anyopaque` casts — the `anyopaque` → `*T` cast is unchecked.

**FFI / C interop**
- `extern fn` calls into C: `strcpy`, `memcpy`, `sprintf` — same risks as [lang/cpp.md](cpp.md).
- `@cImport({ @cInclude("string.h") });` — everything from libc.

**Command execution**
- `std.ChildProcess.init(argv, allocator).spawnAndWait()` — `argv[0]` user-controlled = RCE.  `argv` is an array, so not shell-expanded; but `sh -c user` is always RCE.
- `std.os.execv(path, argv)` — same.

**Integer overflow / truncation**
- Zig separates wrapping / saturating / checked arithmetic: `+%`, `+|`, `@addWithOverflow`.  Plain `+` panics in Debug on overflow but **wraps in ReleaseFast**.
- `@intCast` panics on out-of-range in Debug; **UB in ReleaseFast**.  On a user-controlled input in a ReleaseFast build: silent truncation.
- `@truncate` is always explicit — deliberate narrowing.

**Deserialization / parsers**
- `std.json.parseFromSlice` into a typed struct is safe.  Into a `std.json.Value` tree + walking keys = same prototype-walk-primitive caveat as JS: the `value.get(user_key)` chain is attacker-driven.
- Custom binary parsers advancing a pointer by a user-supplied `u32` length without bounds-checking — OOB read/write.

**Panic → DoS (borderline in-scope)**
- `unreachable` / `@panic` reachable from untrusted input is normally out-of-scope DoS — except in long-running daemons where a crash yields an outage.  Note it; don't file CRITICAL.
- `slice[i]` where `i` is user-derived in a Debug build panics; file as LOW with "crash-only" severity unless downstream state is corrupted.

**Crypto**
- `std.crypto.random.bytes` is fine (backed by `getrandom` on Linux).  `std.rand.DefaultPrng` is NOT cryptographic.
- `std.mem.eql(u8, a, b)` for MAC comparison — timing-unsafe.  Use `std.crypto.utils.timingSafeEql`.

## Tree-sitter seeds (zig)

```scheme
; Pointer casts
(builtin_function
  (builtin_identifier) @b
  (#match? @b "^@ptrCast$|^@bitCast$|^@intCast$|^@truncate$"))

; Common unsafe stdlib entry points
(call_expression
  function: (field_expression field: (identifier) @m)
  (#match? @m "^(alloc|spawn|spawnAndWait|execv|execve|exec)$"))
```

Zig's grammar is still evolving with the language; `ast_describe` before any non-trivial query.
