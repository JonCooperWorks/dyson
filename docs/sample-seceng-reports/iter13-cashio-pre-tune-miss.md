## Security Review: Solana `brrr` Program (Cashio Stablecoin)

Review scope: `programs/brrr/` — a Solana Anchor program for minting/burning $CASH stablecoin collateralized by Saber LP Arrow tokens. Two entry points: `print_cash` (deposit Saber LP → mint $CASH) and `burn_cash` (burn $CASH → withdraw Saber LP).

---

## Checked and Cleared

### Entry Points
- `src/lib.rs:38-41` — `print_cash` handler gated by `#[access_control(ctx.accounts.validate())]`. All accounts validated before execution.
- `src/lib.rs:48-51` — `burn_cash` handler gated by `#[access_control(ctx.accounts.validate())]`. All accounts validated before execution.

### UncheckedAccount Fields (solana-specific constraint audit)
- `src/lib.rs:89` — `issue_authority: UncheckedAccount` with `#[account(mut)]` NOT required. Validation at `src/actions/print_cash.rs:80` via `assert_keys_eq!(self.issue_authority, ISSUE_AUTHORITY_ADDRESS)` — compared against hardcoded PDA derived from `["print", 255]`.
- `src/lib.rs:149` — `withdraw_authority: UncheckedAccount`. Validation at `src/actions/burn_cash.rs:73` via `assert_keys_eq!(self.withdraw_authority, WITHDRAW_AUTHORITY_ADDRESS)` — compared against hardcoded PDA derived from `["burn", 255]`.

### Signer / Owner Checks
- `src/lib.rs:76` — `depositor: Signer<'info>` — Anchor enforces signature check.
- `src/lib.rs:130` — `burner: Signer<'info>` — Anchor enforces signature check.
- `src/actions/print_cash.rs:77` — `assert_keys_eq!(depositor, depositor_source.owner)` — depositor must own source token account.
- `src/actions/burn_cash.rs:67` — `assert_keys_eq!(burner, burned_cash_source.owner)` — burner must own burn source token account.
- `src/actions/print_cash.rs:79` — `mint_destination.mint` matches `crate_token.mint` — prevents mis-mint to wrong token.
- `src/actions/burn_cash.rs:68-70` — burnt cash mint matches `crate_mint`; withdraw destination mint matches `collateral.mint` — prevents cross-mint confusion.

### BrrrCommon Validation (`src/actions/mod.rs:10-23`)
- `bank == collateral.bank` — bank integrity
- `crate_token == crate_collateral_tokens.owner` — crate owns collateral tokens
- `crate_mint == crate_token.mint` — mint consistency
- `crate_collateral_tokens.mint == collateral.mint` — collateral token account holds correct mint
- `collateral.mint == saber_swap.arrow.mint` — saber pool mint matches collateral
- `saber_swap.validate()` (via `src/saber.rs:32-39`) — arrow vendor miner mint, saber pool mint, reserve accounts all validated

### Collateral Hard Cap
- `src/actions/print_cash.rs:16-19` — `current_balance.checked_add(deposit_amount) <= collateral.hard_cap` — prevents collateral over-exposure. Uses checked arithmetic.

### Integer Overflow
- `converter/src/lib.rs` — all arithmetic uses `checked_add/checked_sub/checked_mul/checked_div/checked_pow`. The crate also has `#![deny(clippy::integer_arithmetic)]` which would fail compilation on any unchecked operator.
- `src/actions/print_cash.rs:17` — `checked_add` for hard cap check.
- No raw `+ - * /` operators found anywhere in the codebase (confirmed by `ast_query`).

### CPI Safety
- `src/actions/print_cash.rs:42-60` — `crate_token::cpi::issue` invoked via `CpiContext::new_with_signer` with `ISSUE_AUTHORITY_SIGNER_SEEDS`. Only the program's PDA can authorize mints.
- `src/actions/burn_cash.rs:43-58` — `crate_token::cpi::withdraw` invoked via `CpiContext::new_with_signer` with `WITHDRAW_AUTHORITY_SIGNER_SEEDS`. Only the program's PDA can authorize withdrawals.
- No `invoke`/`invoke_signed` with user-supplied program IDs — all CPIs use typed `Program<'info, T>` references.

### Sysvar Access
- `src/saber.rs:20` — uses `Clock::get()` (safe, fetches from runtime).
- No `from_account_info()` calls on sysvars — no sysvar substitution risk.

### Duplicate Account Mutation
- `PrintCash`: `depositor_source.mint == collateral.mint` vs `mint_destination.mint == crate_mint` — different mints, same account impossible.
- `BurnCash`: `burned_cash_source.mint == crate_mint` vs `withdraw_destination.mint == collateral.mint` — different mints, same account impossible.

### Rounding Analysis (converter/src/lib.rs)
- `scale_lp_to_cash_decimals`: rounds DOWN (checked_div) when `CASH_DECIMALS < lp_mint_decimals` (favors pool); exact when equal; checked_mul (exact) when greater.
- `scale_cash_to_lp_decimals`: rounds DOWN (checked_div) when `CASH_DECIMALS > lp_mint_decimals` (favors pool); exact when equal; checked_mul (exact) when less.
- Rounding direction generally favors the pool, not the attacker. The `stable_swap_math` upstream virtual price calculation is not available in source for full audit, but the decimal scaling layer is sound.

### PDA Bump Seeds
- `src/addresses.rs:9/20` — hardcoded PDAs verified by tests against `Pubkey::find_program_address()` output.
- `src/actions/print_cash.rs:80` — runtime assertion ensures passed account matches expected PDA (not just any PDA).

### Fee Destinations (BurnCash)
- `src/lib.rs:140-145` — `author_fee_destination` / `protocol_fee_destination` validation delegated to `crate_token` CPI callee (noted in `src/actions/burn_cash.rs:71-72`). Accounts are typed `Account<TokenAccount>` — Anchor verifies owner is Token program. Fee calculation and destination authorization occur in the external `crate_token` program; this delegation is a standard CPI pattern.

---

## Dependencies

`dependency_scan` found **0 vulnerable dependencies** across both `Cargo.toml` manifests:
- `brrr`: 9 deps queried (anchor-lang ^0.22, anchor-spl ^0.22, arrow-sunny ^0.2, crate-token ^0.5, stable-swap-anchor ^1.7, vipers ^2, bankman, converter, static-pubkey 1.0.2) — no OSV hits.
- `converter`: 1 dep queried (stable-swap-math ^1.7) — no OSV hits.
- Warning: no `Cargo.lock` present; versions are ranges rather than exact pins. Recommend pinning for reproducible builds.

---

## Remediation Summary

### Immediate (CRITICAL/HIGH)
No findings.

### Short-term (MEDIUM)
No findings.

### Hardening (LOW)
1. `Cargo.toml / converter/Cargo.toml` — No Cargo.lock present. Pin dependency versions exactly for reproducible builds and deterministic security posture.
2. `src/actions/burn_cash.rs:71-72` — Fee destination validation is delegated to external `crate_token` program. Consider adding local assertions that `author_fee_destination.owner` and `protocol_fee_destination.owner` match expected fee collector addresses (if the protocol has designated fee recipients), providing defense-in-depth against changes in the external program's validation logic.