Starting points for Solana programs (Anchor and native SDK) — not exhaustive. Solana's vulnerability classes are dominated by **missing predicates** (forgot to check X before doing Y) rather than source→sink taint; `taint_trace` is still useful for tracing user-controlled data into CPIs and arithmetic, but most findings here live in account-constraint audits that the agent must do by reading instruction handlers and the `#[derive(Accounts)]` / manual `AccountInfo` validation blocks.

Covers two SDK shapes:
- **Anchor** — `use anchor_lang::prelude::*;`, `#[program]`, `#[derive(Accounts)]`, `Context<T>`, `Account<'info, T>`, `Signer<'info>`, `Program<'info, T>`. The framework does most validation for you; bugs come from missing/wrong constraints.
- **Native / raw SDK** — `use solana_program::*;`, `fn process_instruction(program_id, accounts, instruction_data) -> ProgramResult`. The program validates everything by hand; bugs come from forgotten checks.

The same program often mixes both (Anchor entrypoints with native CPI helpers). Apply the full list regardless of detected SDK.

## Entry points (every public instruction handler is attacker-controlled)

- **Anchor**: any `pub fn name(ctx: Context<X>, ...)` inside `#[program] mod foo`. Every parameter after `ctx` is raw instruction data; every account in `X` is an attacker-chosen pubkey. `ctx.accounts.foo` is trusted ONLY for whatever constraints `X` declares.
- **Native**: `process_instruction(program_id, accounts, instruction_data)`. `accounts: &[AccountInfo]` is an attacker-ordered, attacker-chosen slice — the ONLY trusted fact is `program_id` (the runtime passes the real program id). Everything else requires explicit validation in the handler body.

## Vulnerability classes (the canonical list)

**Missing signer check** — the authority account isn't required to sign, so anyone can invoke the "owner-only" path.
- Anchor: a field that should be `Signer<'info>` is typed as `AccountInfo<'info>` or `UncheckedAccount<'info>`; OR `#[account(signer)]` / `has_one = authority` is missing on an account that represents the mutating caller.
- Native: no `if !account.is_signer { return Err(...) }` guard on the authority.

**Missing owner check (account spoofing)** — the program trusts an `AccountInfo` without checking who owns it, so an attacker passes in an account owned by a different program (often the System Program or a program they control) that happens to deserialize into the expected layout.
- Anchor with `Account<'info, T>`: owner is checked implicitly against `T::owner()`. With `AccountInfo` / `UncheckedAccount`: NOT checked. A `#[account(owner = crate::ID)]` constraint is required.
- Native: `if account.owner != program_id { return Err(...) }`. Missing = spoofable.

**Account type confusion / no discriminator check** — manual deserialization of `AccountInfo.data` via `try_from_slice` / `bytemuck::from_bytes` without verifying the type tag at the front of the buffer. Anchor prepends an 8-byte discriminator derived from the account struct name; native programs typically use the first byte as a manual tag.
- Finding shape: `T::try_from_slice(&account.data.borrow()[..])` with no preceding byte/tag check.

**Non-canonical / missing PDA bump** — program computes a PDA via `find_program_address` on the client side and passes in a `bump: u8`, then re-derives on-chain with `create_program_address(seeds, bump)`. An attacker supplies a non-canonical bump that also produces a valid PDA, bypassing the "only the canonical PDA is valid" invariant.
- Anchor: `#[account(seeds = [...], bump = user_supplied)]` without `bump` stored in the account struct and re-validated → vulnerable. Safe: `#[account(seeds = [...], bump)]` (Anchor enforces canonical) or `#[account(seeds = [...], bump = stored.bump)]` where `stored.bump` was written at init time from `ctx.bumps`.
- Native: `Pubkey::create_program_address(seeds_with_attacker_bump, program_id)` without calling `find_program_address` and comparing → vulnerable.

**Arbitrary CPI (program ID not checked)** — program invokes another program via `invoke` / `invoke_signed` and passes along an `AccountInfo` whose pubkey (the target program) came from the instruction's accounts slice without validation. Attacker substitutes a program they control and the CPI runs their code with the current program's authority.
- Anchor: `Program<'info, T>` is typed and checked. `AccountInfo<'info>` passed as the program arg is NOT. `CpiContext::new(attacker_program, ...)` without an address check.
- Native: `invoke(&ix, &[...])` where `ix.program_id` was read from `accounts` without comparing against the expected program id (e.g. `token::ID`).

**Close-account reinitialization (realloc/close + reopen)** — program "closes" an account by transferring out lamports but leaves data in place OR leaves it rent-exempt. Attacker re-uses the same account address in a follow-up instruction, reading the stale data as if freshly initialized.
- Anchor: `#[account(mut, close = receiver)]` is the right pattern. Manual close (`**account.lamports.borrow_mut() = 0;`) without also zeroing data or changing the owner is vulnerable.
- Native: `**acct.lamports.borrow_mut() = 0;` followed by no data wipe and no owner reassignment to the System Program.

**Duplicate account mutation** — the same account is passed in two positions and both mutations apply, producing an illegal state (balance doubled, two counters incremented on what should have been two distinct accounts).
- Anchor: `#[account(mut)]` on two fields that could be the same pubkey without a `constraint = a.key() != b.key()` guard, OR `has_one` checks that pass even when both accounts are the same.
- Native: no `if a.key == b.key { return Err(...) }` guard.

**Missing rent-exempt check on newly-created accounts** — init path creates an account with fewer lamports than rent-exempt minimum for its size; account gets reaped, leaving the program in an inconsistent state; or (older Solana) attacker exploits the garbage collection window. Anchor's `#[account(init, payer = ..., space = ...)]` handles this; manual init with `system_instruction::create_account` does not unless you compute `Rent::get()?.minimum_balance(space)` for lamports.

**Sysvar substitution** — handler reads a sysvar (Clock, Rent, recent_blockhashes, instructions) via its `AccountInfo` from the accounts slice rather than calling `Sysvar::get()`, and doesn't validate the address. Attacker passes an account they control with attacker-written data; program reads fake time or fake rent.
- Finding shape: `Clock::from_account_info(&sysvar_account)` without `sysvar_account.key == &solana_program::sysvar::clock::ID`. Safe: `Clock::get()` (fetches from runtime, not accounts).

**Integer overflow in token math** — `u64` arithmetic on token amounts (lamports, SPL token units, LP shares) without `checked_*` / explicit saturating math. Release builds **do not panic** on overflow; wrap-around silently corrupts balances. `checked_add` / `checked_sub` / `checked_mul` / `checked_div` with `ok_or(Error::Overflow)` is the fix.

**Unchecked `instruction_sysvar` (fake CPI origin)** — program reads the transaction's instruction list via `sysvar::instructions::load_instruction_at_checked` and trusts the claimed origin without validating the caller's program ID. A malicious program in an earlier instruction can make its call look like it came from a trusted program.

**Rounding direction attacks on LP / AMM math** — deposit rounds user shares UP (user gets extra), withdrawal rounds user shares DOWN (pool keeps extra) ⇒ attacker repeatedly deposits 1 unit to farm rounding. Correct direction: deposit rounds DOWN in user's favor, withdrawal rounds UP in pool's favor. Any math on share/amount ratios deserves a manual look.

## Constraint audit (Anchor-specific — open the struct and read every field)

For every `#[derive(Accounts)]` struct, a constraint is implicit-or-explicit on each field. Missing = vulnerability.

- `Signer<'info>` — has signed. Use for authorities.
- `Account<'info, T>` — owner-checked, type-checked (discriminator). Use for program-owned accounts you deserialize.
- `AccountInfo<'info>` / `UncheckedAccount<'info>` — **no checks**. Every use MUST be justified in code by nearby explicit validation; otherwise it's spoofable.
- `Program<'info, T>` — validates the account is the named program. Use this over `AccountInfo` for CPI targets.
- `SystemAccount<'info>` — owned by system program (i.e. a plain wallet).
- `#[account(mut)]` — required for any write. Missing on a write target = runtime error, which is a bug but not a security finding.
- `#[account(init, payer = x, space = y)]` — creates and pays rent. Missing `space` or wrong `space` = reinit attack surface.
- `#[account(seeds = [...], bump)]` — PDA check. `bump` alone (no `=`) uses canonical; `bump = stored.bump` validates against stored canonical; `bump = arg` (from instruction data) is attacker-controlled and dangerous unless also constrained against `find_program_address`.
- `#[account(has_one = authority)]` — the named field on the account must equal the `authority` field in the struct. Missing = authority spoof.
- `#[account(constraint = expr)]` — arbitrary condition. Read the condition; bugs here are common.

## Tree-sitter seeds (rust, Solana-focused)

```scheme
; Anchor program entrypoint (pub fns inside a #[program] mod)
(function_item
  (visibility_modifier) @vis
  name: (identifier) @fn
  parameters: (parameters
    (parameter
      pattern: (identifier) @first
      type: (generic_type
        type: (type_identifier) @ctxty)))
  (#eq? @vis "pub")
  (#eq? @first "ctx")
  (#eq? @ctxty "Context"))

; #[derive(Accounts)] struct (open it — every field is a constraint audit)
(struct_item
  (attribute_item (attribute
    (identifier) @derive
    arguments: (token_tree) @args))
  name: (type_identifier) @name
  (#eq? @derive "derive")
  (#match? @args "Accounts"))

; UncheckedAccount / AccountInfo fields in an Accounts struct — every one
; needs an adjacent #[account(owner = ..., ...)] or code-level check.
(field_declaration
  name: (field_identifier) @f
  type: (generic_type type: (type_identifier) @ty)
  (#match? @ty "^(UncheckedAccount|AccountInfo)$"))

; Manual deserialization without a discriminator check (try_from_slice / bytemuck)
(call_expression function: (scoped_identifier
    name: (identifier) @m)
  (#match? @m "^(try_from_slice|from_bytes|try_from_bytes|load|load_mut)$"))

; invoke / invoke_signed (CPI — check every call for program_id validation)
(call_expression function: [(identifier) @f (scoped_identifier name: (identifier) @f)]
  (#match? @f "^(invoke|invoke_signed)$"))

; create_program_address — if present without find_program_address nearby,
; the bump is likely attacker-supplied.
(call_expression function: (scoped_identifier name: (identifier) @f)
  (#eq? @f "create_program_address"))

; Manual close pattern (lamports zeroed without data wipe)
(assignment_expression
  left: (unary_expression (field_expression
    field: (field_identifier) @f))
  (#eq? @f "lamports"))

; Unchecked integer arithmetic on token amounts — flag `+`/`-`/`*` on u64
; where a `checked_*` call would be safer.  Works as a broad scan.
(binary_expression operator: ["+" "-" "*"])

; Sysvar account taken from accounts slice instead of ::get()
(call_expression function: (scoped_identifier
    path: (identifier) @ty
    name: (identifier) @fn)
  (#match? @ty "^(Clock|Rent|Fees|RecentBlockhashes|EpochSchedule|SlotHashes)$")
  (#eq? @fn "from_account_info"))
```

## Report framing

When filing Solana findings, the `Taint Trace:` block is often less informative than a **constraint-audit snippet**: the `#[derive(Accounts)]` struct at the top, with a short note on which field is missing which check. Include the trace when the vuln has a source→sink shape (arithmetic overflow with user input, CPI target from instruction data), omit it when the vuln is a missing predicate and say so explicitly in Evidence.

Severity defaults:
- Missing signer check on an authority path → **CRITICAL** (anyone can call).
- Missing owner check on an account used for state mutation → **CRITICAL** (account spoofing → arbitrary state).
- Non-canonical PDA bump on an account used for funds custody → **CRITICAL**.
- Arbitrary CPI → **CRITICAL** (delegated authority).
- Integer overflow on funds math → **HIGH** or **CRITICAL** depending on whether overflow reaches a transfer.
- Close-without-wipe → **HIGH** (requires a follow-up ix from attacker).
- Duplicate account mutation → **HIGH** to **CRITICAL** depending on invariant broken.
- Sysvar substitution → **MEDIUM** to **HIGH** depending on what the handler does with the faked data.
