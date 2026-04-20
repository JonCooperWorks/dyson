# Security Review: brrr (Cashio Money Printer)

## CRITICAL

### Unanchored Validation Chain — Every `assert_keys_eq` in `BrrrCommon::validate` and `SaberSwapAccounts::validate` is Relative

- **File:** `src/actions/mod.rs:12-19` and `src/saber.rs:34-37`
- **Evidence:**
  ```
  src/actions/mod.rs:12-19:
      assert_keys_eq!(self.bank, self.collateral.bank);
      assert_keys_eq!(self.crate_token, self.crate_collateral_tokens.owner);
      assert_keys_eq!(self.crate_mint, self.crate_token.mint);
      assert_keys_eq!(self.crate_collateral_tokens.mint, self.collateral.mint);
      // saber swap
      self.saber_swap.validate()?;
      assert_keys_eq!(self.collateral.mint, self.saber_swap.arrow.mint);

  src/saber.rs:34-37:
      assert_keys_eq!(self.arrow.vendor_miner.mint, self.pool_mint);
      assert_keys_eq!(self.saber_swap.pool_mint, self.pool_mint);
      assert_keys_eq!(self.saber_swap.token_a.reserves, self.reserve_a);
      assert_keys_eq!(self.saber_swap.token_b.reserves, self.reserve_b);
  ```
- **Attack Tree:**
  ```
  Instruction accounts (all attacker-supplied) — Solana instruction contains every account pubkey
    └─ src/lib.rs:38 — #[access_control(ctx.accounts.validate())] on print_cash/burn_cash
      └─ src/actions/mod.rs:11 — BrrrCommon::validate: every assert_keys_eq compares one attacker-supplied account to another
        ├─ bank ←→ collateral.bank (relative: both user-supplied pubkeys)
        ├─ crate_token ←→ crate_collateral_tokens.owner (relative)
        ├─ crate_mint ←→ crate_token.mint (relative)
        ├─ crate_collateral_tokens.mint ←→ collateral.mint (relative)
        └─ saber_swap.validate() — all relative: arrow ←→ pool_mint ←→ reserves
      └─ src/actions/print_cash.rs:80 — issue_authority ←→ ISSUE_AUTHORITY_ADDRESS (ANCHORED — but this is the program's own PDA)
        └─ src/actions/print_cash.rs:42-60 — crate_token::cpi::issue() with PDA signer
            ↳ Mints tokens from whatever crate the unanchored chain points to
              └─ src/actions/print_cash.rs:29-39 — LP tokens transferred to attacker-specified crate_collateral_tokens
  ```
- **Classification of all 15 assertions in the validation chain:**

  | # | File:Line | Assertion | Classification |
  |---|-----------|-----------|----------------|
  | 1 | mod.rs:12 | `bank == collateral.bank` | **RELATIVE** — both user-supplied |
  | 2 | mod.rs:13 | `crate_token == crate_collateral_tokens.owner` | **RELATIVE** |
  | 3 | mod.rs:14 | `crate_mint == crate_token.mint` | **RELATIVE** |
  | 4 | mod.rs:15 | `crate_collateral_tokens.mint == collateral.mint` | **RELATIVE** |
  | 5 | mod.rs:19 | `collateral.mint == saber_swap.arrow.mint` | **RELATIVE** |
  | 6 | saber.rs:34 | `arrow.vendor_miner.mint == pool_mint` | **RELATIVE** |
  | 7 | saber.rs:35 | `saber_swap.pool_mint == pool_mint` | **RELATIVE** |
  | 8 | saber.rs:36 | `saber_swap.token_a.reserves == reserve_a` | **RELATIVE** |
  | 9 | saber.rs:37 | `saber_swap.token_b.reserves == reserve_b` | **RELATIVE** |
  | 10 | print_cash.rs:77 | `depositor == depositor_source.owner` | **RELATIVE** |
  | 11 | print_cash.rs:78 | `depositor_source.mint == collateral.mint` | **RELATIVE** |
  | 12 | print_cash.rs:79 | `mint_destination.mint == crate_token.mint` | **RELATIVE** |
  | 13 | print_cash.rs:80 | `issue_authority == ISSUE_AUTHORITY_ADDRESS` | **ANCHORED** — but program's own PDA |
  | 14 | burn_cash.rs:67 | `burner == burned_cash_source.owner` | **RELATIVE** |
  | 15 | burn_cash.rs:73 | `withdraw_authority == WITHDRAW_AUTHORITY_ADDRESS` | **ANCHORED** — but program's own PDA |

  Assertions 13 and 15 anchor only the `issue_authority` and `withdraw_authority` accounts to the program's own PDA — they do **not** anchor the rest of the chain (`bank`, `collateral`, `crate_token`, `crate_mint`, `crate_collateral_tokens`, `saber_swap`, `arrow`, `pool_mint`, `reserve_a`, `reserve_b`) to any canonical protocol state.

- **Taint Trace:** not applicable — this is a **missing-predicate (constraint audit)** finding, not a source→sink taint flow. The vulnerability is the absence of an anchored check on the core accounts. Every `#[derive(Accounts)]` struct and `Validate` impl has been read and every `assert_keys_eq` classified per the table above.
- **Impact:** An attacker permissionlessly creates a `Bank` and `Collateral` (via the `bankman` program), fabricates a self-consistent Saber pool setup, and finds any `CrateToken` on the `crate_token` platform whose authorized issue authority happens to be the brrr program's PDA (seeds `b"print"`). All 15 assertions pass because every check is relative. The program's PDA then signs a CPI to `crate_token::issue`, minting protocol tokens from the target crate against the attacker's worthless collateral. The LP tokens deposited by the attacker are transferred to the attacker-specified `crate_collateral_tokens` (whose owner is the target crate's `CrateToken` PDA — consistent with the relative check). This is the exact mechanism of the March 2022 Cashio exploit (CVE-2022-26891), where $52M was drained.
- **Remediation:** Pin at least one account in the validation chain to a hardcoded canonical address or a PDA derived from `crate::ID` using program-owned seeds. For example, in `BrrrCommon::validate`:

  ```rust
  impl<'info> Validate<'info> for BrrrCommon<'info> {
      fn validate(&self) -> Result<()> {
          assert_keys_eq!(self.crate_token, CANONICAL_CRATE_TOKEN_ADDRESS);  // ADD THIS
          assert_keys_eq!(self.bank, self.collateral.bank);
          assert_keys_eq!(self.crate_collateral_tokens.owner, self.crate_token);
          // ... existing checks ...
      }
  }
  ```

  The anchored check must precede the relative chain so that the canonical `crate_token` is the trust root, and all other accounts are validated against it rather than against each other.

## MEDIUM

### Unvalidated Fee Destinations in BurnCash

- **File:** `src/actions/burn_cash.rs:71-72`
- **Evidence:**
  ```
  src/actions/burn_cash.rs:71-72:
      // author_fee_destination is validated by Crate
      // protocol_fee_destination is validated by Crate
  ```
  The `BurnCash::validate` impl explicitly delegates validation of `author_fee_destination` and `protocol_fee_destination` to the external `crate_token` program, without any local check.
- **Taint Trace:** not run within budget — same-line evidence only. The `burn_cash` instruction accepts these as `Account<'info, TokenAccount>` (type-checked only by SPL) and passes them directly to `crate_token::cpi::withdraw` at line 51-52 via `CpiContext::new_with_signer`.
- **Impact:** If the `crate_token` program's `withdraw` handler accepts the `author_fee_destination` and `protocol_fee_destination` accounts as-is without validating ownership or authority, the caller of `burn_cash` can direct protocol fee tokens to any arbitrary address. This requires the presence of non-zero fees in the crate's configuration to have practical impact. The brrr program's `print_cash` path hardcodes fee destinations to `mint_destination` (line 52-53), which does not have this issue.
- **Remediation:** Add explicit validation in `BurnCash::validate`:
  ```rust
  assert_keys_eq!(self.author_fee_destination.owner, self.burner);
  assert_keys_eq!(self.protocol_fee_destination.owner, self.burner);
  assert_keys_eq!(self.author_fee_destination.mint, self.common.collateral.mint);
  ```

## LOW / INFORMATIONAL

### No Findings.

## Checked and Cleared

- `src/addresses.rs:8-9,19-20` — `ISSUE_AUTHORITY_ADDRESS` and `WITHDRAW_AUTHORITY_ADDRESS` are canonical PDAs derived from `crate::ID` with seeds `b"print"`/`b"burn"`. Tests at lines 33-43 assert `find_program_address` returns the canonical bump (255), confirming the signer seeds use the canonical bump, not an attacker-controllable value. **Cleared: canonical PDA derivation verified by regression test.**
- `src/actions/print_cash.rs:16-20` — Collateral hard cap check uses `current_balance.checked_add(deposit_amount)`. `unwrap_int!` returns `Err` on overflow. **Cleared: no integer overflow — all arithmetic is checked.**
- `converter/src/lib.rs:39-61` — `scale_lp_to_cash_decimals` and `scale_cash_to_lp_decimals` use `.checked_mul` / `.checked_div` / `.checked_sub` exclusively. `?` propagates overflow as `None`. **Cleared: all math operations use checked arithmetic.**
- `src/actions/burn_cash.rs:24-27` — `current_balance >= withdraw_pool_token_amount` is a safe u64 comparison; both operands are validated outputs of checked arithmetic. **Cleared.**
- `src/lib.rs:89` — `issue_authority: UncheckedAccount<'info>` is validated by `assert_keys_eq!(self.issue_authority, ISSUE_AUTHORITY_ADDRESS)` in `PrintCash::validate`. **Cleared: untyped, but address-pinned.**
- `src/lib.rs:149` — `withdraw_authority: UncheckedAccount<'info>` is validated by `assert_keys_eq!(self.withdraw_authority, WITHDRAW_AUTHORITY_ADDRESS)` in `BurnCash::validate`. **Cleared: untyped, but address-pinned.**
- `src/lib.rs:39-41, 49-51` — Both `print_cash` and `burn_cash` use `#[access_control(ctx.accounts.validate())]`, executing the `Validate` impl before handler logic. **Cleared: standard Anchor access control.**
- `src/saber.rs:11-29` — `TryFrom<&SaberSwapAccounts> for CashSwap` reads from Anchor-typed accounts (`pool_mint: Account<Mint>`, etc.). The conversion does not introduce unsafe behavior; it merely copies fields. **Cleared.**
- `src/actions/mod.rs:10-22` — `BrrrCommon::validate` is the core of the CRITICAL finding already filed above. As an individual check, it is not vulnerable in isolation; the vuln is the chain of relative-only checks across all three `Validate` impls. **Cleared as standalone, filed as part of CRITICAL finding.**
- `src/lib.rs:115` — `token_program: Program<'info, Token>` — Anchor validates this is the SPL Token program by ID. **Cleared.**
- `src/lib.rs:118` — `crate_token_program: Program<'info, CrateToken>` — Anchor validates this is the crate_token program by ID. **Cleared: cannot be spoofed.**

## Dependencies

`dependency_review` found **0 vulnerabilities** across 10 resolved dependencies in 2 Cargo.toml files, but flags two important caveats:
- **No `Cargo.lock` committed** — semver ranges (`anchor-lang ^0.22`, `vipers ^2`, `anchor-spl ^0.22`) could resolve to any version within the range. Initial point releases (e.g. `anchor-lang 0.22.0`) had known security issues patched in later versions. The scan resolved to latest-compatible; the actual build may differ.
- No vulnerable dependencies detected at the resolved versions.

**Commit a `Cargo.lock` and re-scan to get deterministic results.**

## Remediation Summary

### Immediate (CRITICAL)
1. `src/actions/mod.rs:12` — Pin at least one account (`crate_token` or `bank`) to a hardcoded canonical address BEFORE the relative validation chain, to anchor the trust root and prevent the Cashio-style unanchored chain attack.

### Short-term (MEDIUM)
1. `src/actions/burn_cash.rs:71` — Add explicit `assert_keys_eq!` checks for `author_fee_destination` and `protocol_fee_destination` ownership and mint instead of delegating entirely to the external `crate_token` program.

### Hardening (LOW)
- No findings.