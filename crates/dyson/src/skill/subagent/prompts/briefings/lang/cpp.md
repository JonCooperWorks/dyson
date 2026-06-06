Starting points for C / C++ — not exhaustive. Memory-safety bugs dominate; follow the taint carefully. Novel sinks outside this list are still in scope.

## Sinks

**Memory safety (primary attack surface)**
- `strcpy`, `strcat`, `sprintf`, `gets` — unbounded writes.  Any use with external input is a buffer overflow.
- `memcpy(dst, src, user_len)`, `memmove(dst, src, user_len)` — OOB write if `user_len` isn't validated against `sizeof(dst)`.
- `alloca(user_len)` / VLAs with user-sized length — stack smash.
- `malloc(user_a * user_b)` — integer overflow on multiplication yields a small allocation followed by large `memcpy` → heap overflow.  Use `calloc(a, b)` which checks for overflow.
- `realloc(p, new_size)` where `new_size == 0` is implementation-defined (may free `p`); and the common `p = realloc(p, n)` pattern leaks `p` on failure.
- `*(T*)ptr` with unaligned `ptr` — UB on strict-alignment targets.
- C++ `reinterpret_cast<T*>(bytes)` on network bytes — type-punning UB.
- `std::vector::operator[]` / C array index `a[i]` with user `i` — OOB; prefer `.at(i)` which bounds-checks.
- `strncpy(dst, src, n)` does NOT null-terminate if `src` ≥ `n` — subsequent `strlen(dst)` walks off the end.

**Integer bugs**
- Signed overflow in `int` arithmetic is UB — the compiler may delete bounds checks.  `if (a + b < a)` is UB, not a check.
- Narrowing from `size_t` → `int` for a length → sign flip; then `memcpy` reads astronomically.
- `malloc(strlen(user) + 1)` — `strlen` on a non-null-terminated buffer walks forever.

**Command execution**
- `system(cmd)`, `popen(cmd, "r")`, `execl`/`execv`/`execve` — RCE when `cmd` is user-controlled.
- `putenv(user_str)` — environment inheritance downstream.

**Format strings**
- `printf(user_str)`, `sprintf(dst, user_str)`, `syslog(prio, user_str)` — format-string attacks (`%n` writes, leaking stack).  Always pass user data as an arg: `printf("%s", user)`.

**Use-after-free / double-free**
- Returning / storing pointers to `alloca`d memory, stack locals, or freed heap chunks.
- `delete` on an object held by another owner.
- `std::unique_ptr` + raw pointer aliasing — one frees, the other dereferences.

**Race conditions with security impact**
- TOCTOU: `stat(path)` then `open(path)` — symlink race.  Use `openat` with a pre-opened dir fd + `O_NOFOLLOW`.
- Signal handlers doing non-async-signal-safe work (malloc, printf) — UB, can corrupt state.

**Crypto / randomness**
- `rand()` for keys / nonces — predictable.  Use `getrandom(2)` / `CryptGenRandom` / `arc4random_buf`.
- Naive `memcmp` on MACs — timing-unsafe.  Use a constant-time compare.

**Path / file**
- `open(user_path, ...)` with `..` — traversal; use `openat` + `O_NOFOLLOW` or realpath + prefix check.
- Zip libraries (libarchive, minizip) with user-controlled entry names → Zip Slip.

**Network / parser**
- `recv(sock, buf, user_len, 0)` where `user_len > sizeof(buf)` — OOB write.
- Custom binary parsers that advance a pointer by a user-supplied length without bounds-checking.
- `fread(buf, 1, user_nitems, fp)` with `user_nitems > sizeof(buf)`.

## Tree-sitter seeds (c / cpp)

```scheme
; Unsafe string / memory functions — any call worth inspection
(call_expression
  function: (identifier) @f
  (#match? @f "^(strcpy|strcat|sprintf|gets|memcpy|memmove|alloca|system|popen)$"))

; Format-string call: first arg is not a string literal
(call_expression
  function: (identifier) @f
  (#match? @f "^(printf|fprintf|sprintf|snprintf|syslog)$")) @format

; Reinterpret / c-style cast
(cast_expression) @cast        ; c
(reinterpret_cast_expression) @rcast  ; cpp

; new / delete
(new_expression) @new
(delete_expression) @delete
```
