Starting points for Nix — not exhaustive. Nix is a pure-ish lazy configuration language; most of its "attack surface" lives at evaluation time (builders + fetchers + impure escapes). Novel sinks outside this list are still in scope.

## Sinks

**Impure escape hatches**
- `builtins.currentSystem`, `builtins.getEnv "NAME"` — pull values from the evaluator's environment.  Committed expressions that rely on `getEnv` pull secrets from the build host.
- `--impure` mode lets all of `builtins.getEnv`, `builtins.currentTime`, `builtins.readFile` read outside the store; a committed flake that requires `--impure` imports build-host state into the build.
- `import <nixpkgs> {}` with `<nixpkgs>` from an unpinned channel — attacker of the host's channel config changes what the build uses.

**Fetchers**
- `fetchurl { url = "http://..."; sha256 = ""; }` — empty hash disables verification; fetcher uses whatever's at the URL.  Same for `fetchgit`, `fetchTarball`, `fetchzip`.
- `builtins.fetchGit { url = ...; rev = ...; }` without `rev` pinned — attacker who controls the upstream branch picks what you build.
- `fetchFromGitHub { owner; repo; rev; sha256; }` — same story.  `rev` must be a full SHA, not a tag / branch.
- `lib.fakeSha256` / `lib.fakeHash` in a committed expression — left-over placeholder; build doesn't verify.

**Builder scripts**
- `stdenv.mkDerivation { buildCommand = "... ${user_attr} ..."; }` — Nix-string interpolation into a bash script; the bash script inherits the usual shell-injection concerns if `user_attr` is a flake input or an impure read.
- `writeScript` / `writeShellScript` with interpolation from flake inputs.

**Eval-time evaluation of user strings**
- `builtins.fromJSON user_str` — safe structurally.
- `builtins.fromTOML user_str` — safe structurally.
- `builtins.exec [ "command" "arg" ]` — ONLY when evaluator is run with `--allow-unsafe-native-code-during-evaluation` (requires explicit opt-in).  But any committed expression using it is a finding (elevates eval to RCE on opt-in hosts).

**Committed secrets**
- Plain-text `api_key = "sk-..."` / `password = "..."` in a `.nix` file — finding (source-committed secret).
- `sops-nix` / `agenix` encrypted files are fine; check the encryption is actually enabled (the `.age` / `.enc` extension isn't proof).

**NixOS module surface (when used for system config)**
- `services.<foo>.extraConfig = user_string` — often a bash / ini / systemd fragment that can inject unescaped into the final config file.
- `security.sudo.extraRules` with user-templated commands — privilege escalation if the template allows arguments.
- `environment.etc."foo".text = user` — arbitrary content into `/etc/foo`.

**Impure reads**
- `builtins.readFile ./path` — fine for in-repo paths.  `readFile "/etc/passwd"` in an impure build leaks host state.

## Tree-sitter seeds (nix)

```scheme
; Function application with a builtin
(apply_expression
  function: (variable_expression (identifier) @f)
  (#match? @f "^(fetchurl|fetchgit|fetchTarball|fetchzip|fromJSON|fromTOML|readFile|getEnv|exec)$"))

; Attribute selection: builtins.<x>
(select_expression
  (variable_expression (identifier) @m)
  (attrpath (identifier) @n)
  (#eq? @m "builtins"))
```

Nix's grammar models expressions, not call-graph flow — `taint_trace`'s call-based indexing is near-useless here.  Rely on `ast_query` + `search_files` for Nix reviews.
