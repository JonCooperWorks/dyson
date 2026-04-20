## Security Review: brrr ($CASH mint/burn Solana program)

---

## HIGH

### Unanchored validation chain in `BrrrCommon` enables arbitrary bank/collateral injection
- **File:** `src/actions/mod.rs:10-22`
- **Evidence:**
  ```rust
  impl<'info> Validate<'info> for BrrrCommon<'info> {
      fn validate(&self) -> Result<()> {
          assert_keys_eq!(self.bank, self.collateral.bank);
          assert_keys_eq!(self.crate_token, self.crate_collateral_tokens.owner);
          assert_keys_eq!(self.crate_mint, self.crate_token.mint);
          assert_keys_eq!(self.crate_collateral_tokens.mint, self.collateral.mint);

          // saber swap
          self.saber_swap.validate()?;
          assert_keys_eq!(self.collateral.mint, self.saber_swap.arrow.mint);

          Ok(())
      }
  }
  ```
- **Attack Tree:**
  ```
  src/lib.rs:39 — print_cash entrypoint accepts attacker-chosen accounts
    └─ src/actions/mod.rs:10 — BrrrCommon::validate() runs, but ALL assert_keys_eq! are relative-only
      └─ src/lib.rs:95-109 — bank, collateral, crate_token fields are passed from attacker with no anchor to canonical address
        └─ src/actions/print_cash.rs:17 — hard_cap check passes because hard_cap is read from the attacker's Collateral
          └─ src/actions/print_cash.rs:42 — crate_token::cpi::issue mints $CASH from attacker-selected crate
  ```
- **Impact:** This is the **Cashio vulnerability pattern** ($52M exploit, March 2022). The `Validate` impl for `BrrrCommon` contains six `assert_keys_eq!` comparisons, all of which are **relative** — each compares two fields from the input accounts. No comparison anchors to a hardcoded pubkey constant (e.g., `ISSUE_AUTHORITY_ADDRESS`), a PDA derived from `crate::ID`, or a globally-trusted account. An attacker who can permissionlessly create a `Bank` and `Collateral` through the `bankman` program can wire a self-consistent fake chain, pass this validation entirely, and deposit attacker-controlled LP tokens into a crate, receiving $CASH minted by the crate authority CPI. The `hard_cap` check on line 17 of `src/actions/print_cash.rs` reads `self.common.collateral.hard_cap` — an attacker can create a Collateral with an arbitrarily high hard_cap, bypassing that guard.

  While the `crate_token::issue` CPI is gated by PDA signer seeds (verified independently by the crate_token program), the attacker's control over WHICH `Collateral` is used matters because: (1) the hard_cap check uses the attacker's collateral, (2) the LP token value calculation uses the attacker's saber swap data, and (3) the crate_token program's own authority validation may not enforce that only canonical banks/collateral can trigger issuance — the crate_token delegates authorization to the CPI signer, which this program provides for ANY input that passes validation.
- **Remediation:** Add an anchored comparison to `BrrrCommon::validate()` that ties at least one field to a canonical address. For example:

  ```rust
  impl<'info> Validate<'info> for BrrrCommon<'info> {
      fn validate(&self) -> Result<()> {
          // NEW: anchor the chain to the crate_token's hardcoded constants
          assert_keys_eq!(self.bank, /* CANONICAL_BANK_ADDRESS */);
          // ... rest remains
      }
  }
  ```

  Or restructure to a single-bank-per-program design where `bank` is a PDA of `crate::ID`, preventing attacker-permissioned creation.

### `UncheckedAccount` fields verified only by runtime `assert_keys_eq` — missing compile-time safety
- **File:** `src/lib.rs:89` and `src/lib.rs:149`
- **Evidence:**
  ```rust
  // Line 89
  pub issue_authority: UncheckedAccount<'info>,

  // Line 149
  pub withdraw_authority: UncheckedAccount<'info>,
  ```
- **Attack Tree:**
  ```
  src/lib.rs:89 — issue_authority is UncheckedAccount (no Anchor type/discriminator check)
    └─ src/actions/print_cash.rs:80 — assert_keys_eq checks it at runtime against ISSUE_AUTHORITY_ADDRESS
      └─ If assert_keys_eq is ever reordered, removed, or the validate() is bypassed by a future dev,
         an attacker can pass any account and the PDA signer seeds would still sign
  ```
- **Impact:** Both `issue_authority` and `withdraw_authority` are `UncheckedAccount<'info>`, meaning Anchor performs zero validation on them at deserialization time. The security property relies entirely on Vipers `assert_keys_eq!` in the `Validate` impls. This is documented with `// CHECK: this is handled by Vipers` but provides no structural protection if a future refactoring removes or reorders the validation call. Using `AccountInfo<'info>` with `#[account(address = ISSUE_AUTHORITY_ADDRESS)]` would provide compile-time guarantees and eliminate the `UncheckedAccount` footgun. The current validation (`src/actions/print_cash.rs:80` and `src/actions/burn_cash.rs:73`) does anchor these addresses, but the pattern is fragile.
- **Remediation:** Replace `UncheckedAccount<'info>` with explicit Anchor constraints:

  ```rust
  #[account(address = ISSUE_AUTHORITY_ADDRESS)]
  pub issue_authority: AccountInfo<'info>,

  #[account(address = WITHDRAW_AUTHORITY_ADDRESS)]
  pub withdraw_authority: AccountInfo<'info>,
  ```

  Or with Vipers 2's `#[vipers::key("static_pubkey_string")]` attribute syntax. This moves the security check from runtime validation into Anchor's deserialization pipeline.

---

## MEDIUM

### `unwrap_int!` macro masks arithmetic failures — early return on zero prevents underflow but hides edge cases
- **File:** `src/actions/print_cash.rs:17`
- **Evidence:**
  ```rust
  require!(
      unwrap_int!(current_balance.checked_add(deposit_amount))
          <= self.common.collateral.hard_cap,
      CollateralHardCapHit
  );
  ```
- **Attack Tree:**
  ```
  src/actions/print_cash.rs:9 — print_cash entrypoint with deposit_amount from user
    └─ src/actions/print_cash.rs:15 — current_balance read from crate_collateral_tokens
      └─ src/actions/print_cash.rs:17 — unwrap_int! on checked_add; if checked_add returns None,
         unwrap_int! triggers a Rust panic (Anchor error), aborting the transaction
  ```
- **Impact:** `unwrap_int!` on `checked_add` panics rather than returning an Anchor `Result`, which is correct behavior (prevents overflow). However, the `hard_cap` value is read from `self.common.collateral.hard_cap` — an `Account<'info, Collateral>` owned by `bankman`. Anchor verifies the owner is `bankman` and the discriminator matches, but the attacker controls WHICH `Collateral` account is provided. The `BrrrCommon::validate()` only does relative `assert_keys_eq` checks, not anchoring to a canonical address. An attacker can supply a Collateral with an arbitrarily high `hard_cap`, making the check trivially pass. This is a secondary vector for the HIGH finding above.
- **Remediation:** Same as HIGH finding — anchor the chain validation to canonical addresses. With anchored validation, the hard_cap check would read from the canonical Collateral, not an attacker-chosen one.

---

## LOW / INFORMATIONAL

No standalone LOW findings.

---

## Checked and Cleared

- `src/actions/print_cash.rs:77-78` — `depositor` ownership and mint validation are relative but correctly constrain that the depositor owns the source token account and the source holds the collateral mint
- `src/actions/print_cash.rs:79-80` — `mint_destination` mint checked against crate mint; `issue_authority` anchored to hardcoded `ISSUE_AUTHORITY_ADDRESS` at runtime
- `src/actions/burn_cash.rs:67-68` — `burner` ownership and mint validation relative but correct
- `src/actions/burn_cash.rs:70` — `withdraw_destination` mint checked against collateral mint
- `src/actions/burn_cash.rs:73` — `withdraw_authority` anchored to hardcoded `WITHDRAW_AUTHORITY_ADDRESS`
- `src/saber.rs:34-37` — Saber validation chain: arrow miner mint, saber swap pools, reserves all cross-checked relative to each other; Anchor `Account<'info, Arrow>` and `Account<'info, SwapInfo>` provide type/discriminator checks
- `converter/src/lib.rs:26-61` — All arithmetic uses `checked_*` methods; `#![deny(clippy::integer_arithmetic)]` enforced
- `src/lib.rs:39-51` — Anchor `#[access_control(ctx.accounts.validate())]` gates execution; no bypass possible
- `src/addresses.rs:8-27` — PDA addresses and bump seeds are deterministic tests; no hardcoded secrets (these are public PDA addresses, not keys)
- `src/events.rs:8-39` — Event structs only emit data; no sink
- `src/actions/burn_cash.rs:17-18` — `calculate_pool_tokens_for_cash` returns `Option<u64>`; `unwrap_int!` panics on `None` (safe underflow prevention)

---

## Dependencies

No vulnerable dependencies found (per OSV query from dependency_review subagent). All dependencies resolved within version constraints:
- `anchor-lang ^0.22` — no known CVE at queried version
- `vipers ^2` — no known CVE
- `bankman`, `crate-token`, `arrow-sunny`, `stable-swap-anchor` — no advisories found

Recommendation: generate a `Cargo.lock` to pin exact versions and re-scan.

---

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `src/actions/mod.rs:10-22` — Add an anchored comparison in `BrrrCommon::validate()` to a canonical address; this is the Cashio vulnerability pattern that led to a $52M exploit.
2. `src/lib.rs:89,149` — Replace `UncheckedAccount<'info>` with `AccountInfo<'info>` + `#[account(address = ...)]` attribute for compile-time safety on authority accounts.

### Short-term (MEDIUM)
1. `src/actions/print_cash.rs:17` — `hard_cap` check uses attacker-controlled `Collateral`; address via the same anchored-chain fix from HIGH #1.

### Hardening (LOW)
1. No additional hardening items identified.