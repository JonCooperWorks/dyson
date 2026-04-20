Now I have all the data I need. This is an educational codebase (`coral-xyz/sealevel-attacks`) where each numbered directory demonstrates one vulnerability class. The insecure variants contain the actual vulnerabilities. Let me now write the report.

## Security Review: Solana / Anchor Educational Program Collection

This codebase is `coral-xyz/sealevel-attacks` — an educational collection of Solana Anchor programs, each demonstrating a distinct vulnerability class via `insecure` / `recommended` / `secure` variants. 11 programs were reviewed, 10 of which contain exploitable vulnerabilities in their `insecure` variant. All findings are in the `insecure/` subdirectory programs.

---

## CRITICAL

### 1. Missing Signer Authorization — any caller accepted as authority
- **File:** `0-signer-authorization/insecure/src/lib.rs:16`
- **Evidence:**
  ```rust
  #[derive(Accounts)]
  pub struct LogMessage<'info> {
      authority: AccountInfo<'info>,
  }
  ```
- **Attack Tree:**
  ```
  External caller -> invokes log_message() -> authority account typed as AccountInfo (not Signer)
    └─ No is_signer check anywhere in the handler body
      └─ Any pubkey can be passed as authority and accepted without signing
  ```
- **Impact:** Any caller can invoke this instruction with an arbitrary pubkey as "authority". While this example only logs a message, in real-world use the same pattern lets an attacker impersonate any authority, steal funds, or mutate state.
- **Exploit:** Call the instruction with `authority` set to the victim's pubkey — no signature required.
- **Remediation:** Change `AccountInfo<'info>` to `Signer<'info>`:
  ```rust
  #[derive(Accounts)]
  pub struct LogMessage<'info> {
      authority: Signer<'info>,
  }
  ```

### 2. Type Cosplay — deserializing attacker-controlled account data without discriminator check
- **File:** `3-type-cosplay/insecure/src/lib.rs:11`
- **Evidence:**
  ```rust
  let user = User::try_from_slice(&ctx.accounts.user.data.borrow()).unwrap();
  ```
  No `#[account(owner = ctx.program_id)]` constraint on the `user` account (line 25), only a runtime owner check at line 12. No discriminant/tag validation before deserialization.
- **Attack Tree:**
  ```
  External caller -> passes account with crafted Borsh bytes -> try_from_slice deserializes as User
    └─ Owner is checked at line 12 but account content is arbitrary caller-supplied data
      └─ Attacker passes a Metadata account (same owner, same program) with authority = attacker's key
        └─ authority check at line 15 passes, attacker gains unauthorized access
  ```
- **Impact:** An attacker can pass any program-owned account whose Borsh layout overlaps with `User::authority` at offset 0 (e.g. the `Metadata` struct defined at line 35-37). No type tag/discriminant is checked. This allows authority impersonation and arbitrary state reads.
- **Exploit:** Create a `Metadata` account with the first 32 bytes set to the attacker's pubkey, pass it as `user` — deserializes as `User { authority: attacker }`, authority check passes.
- **Remediation:** Use Anchor's `Account<'info, User>` instead of `AccountInfo<'info>` (which provides discriminator checking) or add a manual discriminant check:
  ```rust
  #[derive(Accounts)]
  pub struct UpdateUser<'info> {
      #[account(owner = crate::ID)]
      user: AccountInfo<'info>,
      authority: Signer<'info>,
  }
  // And verify discriminant in handler:
  if user.discriminant != AccountDiscriminant::User { return Err(...); }
  ```

### 3. Arbitrary CPI — token program ID not validated, attacker-supplied program accepted
- **File:** `5-arbitrary-cpi/insecure/src/lib.rs:11-13`
- **Evidence:**
  ```rust
  solana_program::program::invoke(
      &spl_token::instruction::transfer(
          ctx.accounts.token_program.key,  // ← attacker-controlled program ID
  ```
- **Attack Tree:**
  ```
  External caller -> sets token_program to attacker-controlled program
    └─ invoke() calls SPL token transfer with attacker's program as the "token program"
      └─ Attacker program receives authority to sign and approve transfers without validation
  ```
- **Impact:** The `token_program` account is passed as `AccountInfo<'info>` with no check against `spl_token::ID`. An attacker can supply a malicious program as `token_program`, causing the CPI to execute attacker code with the program's PDA seeds (via `invoke_signed`), stealing funds or authorizing arbitrary transfers.
- **Exploit:** Deploy a fake SPL token program, pass its pubkey as `token_program` — the CPI invokes the attacker program instead.
- **Remediation:** Add explicit program ID validation:
  ```rust
  if ctx.accounts.token_program.key != spl_token::ID {
      return Err(ProgramError::IncorrectProgramId);
  }
  // Or use Program<'info, Token> instead of AccountInfo<'info>
  ```

### 4. Reinitialization — account deserialized without checking if already initialized
- **File:** `4-initialization/insecure/src/lib.rs:12`
- **Evidence:**
  ```rust
  pub fn initialize(ctx: Context<Initialize>) -> ProgramResult {
      let mut user = User::try_from_slice(&ctx.accounts.user.data.borrow()).unwrap();
      user.authority = ctx.accounts.authority.key();
  ```
  No check for whether the account already has an authority set.
- **Attack Tree:**
  ```
  External caller -> calls initialize on an already-initialized account
    └─ deserializes existing User struct
      └─ Overwrites authority with caller's key (line 14)
        └─ Caller becomes the new authority of someone else's account
  ```
- **Impact:** An attacker can reinitialize an already-initialized account, stealing ownership by overwriting the `authority` field. In any staking, vault, or token protocol, this is a full account takeover.
- **Exploit:** Call `initialize()` on a `User` account already owned by another user with the attacker's pubkey as signer — attacker becomes the new authority.
- **Remediation:** Check initialization state before writing:
  ```rust
  // Add initialized flag to struct:
  #[derive(BorshSerialize, BorshDeserialize)]
  pub struct User {
      is_initialized: bool,
      authority: Pubkey,
  }
  // In handler:
  if user.is_initialized { return Err(ProgramError::InvalidAccountData); }
  user.is_initialized = true;
  ```
  Or use Anchor's `#[account(zero)]` constraint which checks the account is zeroed out.

### 5. PDA Sharing — vault PDA seeds derived from `pool.mint` instead of `pool.withdraw_destination`
- **File:** `8-pda-sharing/insecure/src/lib.rs:12`
- **Evidence:**
  ```rust
  let seeds = &[ctx.accounts.pool.mint.as_ref(), &[ctx.accounts.pool.bump]];
  ```
- **Attack Tree:**
  ```
  Attacker creates TokenPool with pool.mint = same mint as victim's pool
    └─ pool.withdraw_destination = attacker's token account
      └─ PDA seeds = (mint, bump) — same as victim's vault PDA
        └─ Attacker calls withdraw_tokens() with attacker's withdraw_destination
          └─ CPI signs vault transfer with matching PDA seeds — drains victim's vault
  ```
- **Impact:** The vault PDA is uniquely determined by `(mint, bump)`. Any pool with the same `mint` and `bump` shares the same vault PDA. An attacker can create a malicious pool configuration, set `withdraw_destination` to their own account, and drain the vault through the shared PDA.
- **Exploit:** Create a pool with `vault = victim_vault`, `mint = victim_mint`, `withdraw_destination = attacker_tok_acct`, and the correct `bump`. Call `withdraw_tokens()` — PDA seeds match, CPI signs, vault drained.
- **Remediation:** Include `withdraw_destination` in the PDA seeds so each pool has a unique vault:
  ```rust
  let seeds = &[
      ctx.accounts.pool.withdraw_destination.as_ref(),
      &[ctx.accounts.pool.bump],
  ];
  ```

### 6. Non-canonical Bump Seed — attacker supplies arbitrary bump validated only against create_program_address
- **File:** `7-bump-seed-canonicalization/insecure/src/lib.rs:11`
- **Evidence:**
  ```rust
  pub fn set_value(ctx: Context<BumpSeed>, key: u64, new_value: u64, bump: u8) -> ProgramResult {
      let address =
          Pubkey::create_program_address(&[key.to_le_bytes().as_ref(), &[bump]], ctx.program_id)?;
  ```
  `create_program_address` accepts ANY valid bump, not just the canonical one from `find_program_address`.
- **Attack Tree:**
  ```
  External caller -> finds non-canonical bump that also produces a valid PDA for the key
    └─ Passes attacker-controlled bump to set_value()
      └─ create_program_address succeeds with non-canonical bump
        └─ Attacker writes new_value to the canonical Data account (same pubkey)
  ```
- **Impact:** For any set of seeds, multiple bump values can produce valid PDAs (canonical + up to 255 non-canonical). The canonical bump from `find_program_address` is the one the program should use. Using `create_program_address` with an attacker-supplied bump allows the attacker to target the canonical account's pubkey with a non-canonical bump, bypassing initialization protections or writing to existing data.
- **Exploit:** Brute-force bump values until a non-canonical one produces the same PDA as the canonical one for the target key, then call `set_value()` to overwrite data.
- **Remediation:** Use `find_program_address` to verify the canonical bump:
  ```rust
  let (address, expected_bump) =
      Pubkey::find_program_address(&[key.to_le_bytes().as_ref()], ctx.program_id);
  if address != ctx.accounts.data.key() || expected_bump != bump {
      return Err(ProgramError::InvalidArgument);
  }
  ```
  Or use Anchor's `#[account(seeds = [...], bump)]` which enforces canonical bumps.

---

## HIGH

### 7. Account Reinitialization After Close — zeroed lamports without data wipe
- **File:** `9-closing-accounts/insecure/src/lib.rs:10-18`
- **Evidence:**
  ```rust
  **ctx.accounts.account.to_account_info().lamports.borrow_mut() = 0;
  Ok(())  // ← data buffer left intact, account not reassigned to system program
  ```
- **Attack Tree:**
  ```
  Attacker calls close() on their account -> lamports set to 0, data untouched
    └─ Account still owned by the program (owner field unchanged)
      └─ Caller reinitializes account in same tx or next tx -> reads stale data
  ```
- **Impact:** Closing an account by zeroing lamports alone does not reassign ownership to the System Program. The account remains owned by the program and its data buffer is intact. An attacker (or anyone) can reinitialize the account in a follow-up instruction, inheriting the previous state — enabling double-spends, balance reuse, or state replay attacks.
- **Exploit:** Call `close()` to zero lamports, then call `initialize()` on the same account — the old data remains, potentially causing the program to operate on stale balances/authority.
- **Remediation:** Zero the data buffer and write the `CLOSED_ACCOUNT_DISCRIMINATOR` after draining lamports, as shown in the `secure` variant:
  ```rust
  let mut data = account.try_borrow_mut_data()?;
  for byte in data.deref_mut().iter_mut() { *byte = 0; }
  cursor.write_all(&CLOSED_ACCOUNT_DISCRIMINATOR).unwrap();
  ```
  Best practice: use Anchor's `#[account(close = destination)]` which handles this atomically.

### 8. Arbitrary Sysvar Account Substitution — sysvar passed from caller without address validation
- **File:** `10-sysvar-address-checking/insecure/src/lib.rs:17`
- **Evidence:**
  ```rust
  #[derive(Accounts)]
  pub struct CheckSysvarAddress<'info> {
      rent: AccountInfo<'info>,
  }
  ```
  No check that `ctx.accounts.rent.key()` equals `solana_program::sysvar::rent::ID`.
- **Attack Tree:**
  ```
  External caller -> passes arbitrary AccountInfo as "rent"
    └─ Program reads arbitrary account data, trusting it is the rent sysvar
      └─ In real programs this data would drive rent calculations, exemption checks, or fee logic
  ```
- **Impact:** While this specific handler only logs the sysvar key, any real handler using the `rent` account to make decisions (rent exemption size calculations, fee adjustments, account sizing) would read attacker-controlled data. An attacker could pass an account with crafted "rent" parameters to bypass rent exemptions, manipulate account sizing, or cause the program to operate with incorrect economic parameters.
- **Exploit:** Pass a program-controlled account as `rent` with crafted sysvar data — if the handler uses any field from the account, it reads attacker-controlled values.
- **Remediation:** Validate the address against the known sysvar constant:
  ```rust
  if ctx.accounts.rent.key != solana_program::sysvar::rent::ID {
      return Err(ProgramError::InvalidArgument);
  }
  // Or use Sysvar::get() instead of passing it as an account:
  let rent = Rent::get()?;
  ```

### 9. Owner Check Missing — SPL token account owner verified but account source not validated
- **File:** `2-owner-checks/insecure/src/lib.rs:12-14`
- **Evidence:**
  ```rust
  let token = SplTokenAccount::unpack(&ctx.accounts.token.data.borrow())?;
  if ctx.accounts.authority.key != &token.owner {
  ```
  The `token` account is `AccountInfo<'info>` — any byte sequence can be passed and will be unpacked as `SplTokenAccount`. There is no check that the account is actually owned by the SPL token program.
- **Attack Tree:**
  ```
  External caller -> passes account with crafted 165 bytes matching SplTokenAccount layout
    └─ account.owner field = attacker's pubkey (crafted in byte layout)
      └─ Unpack succeeds, authority check passes
        └─ Attaker's fake "token account" accepted as legitimate
  ```
- **Impact:** Without checking `ctx.accounts.token.owner == spl_token::ID`, any attacker can pass a raw byte buffer that deserializes as a `SplTokenAccount` with the `owner` field set to their pubkey and an arbitrary `amount`. The program would act on fake balance data, enabling balance spoofing, unauthorized operations, or theft.
- **Exploit:** Craft 165 bytes that deserialize as `SplTokenAccount { owner: attacker_key, amount: u64::MAX, ... }`, pass as `token` — unpack succeeds, authority check passes, program proceeds with fake balance.
- **Remediation:** Check the account owner before unpacking:
  ```rust
  if ctx.accounts.token.owner != &spl_token::ID {
      return Err(ProgramError::IncorrectProgramId);
  }
  ```
  Or better, use Anchor's `Account<'info, TokenAccount>` from `anchor_spl::token` which enforces this.

### 10. Account Data Mismatch — SPL token account data not validated from token program
- **File:** `1-account-data-matching/insecure/src/lib.rs:12`
- **Evidence:**
  ```rust
  pub fn log_message(ctx: Context<LogMessage>) -> ProgramResult {
      let token = SplTokenAccount::unpack(&ctx.accounts.token.data.borrow())?;
  ```
  Same issue as Finding #9: `token` is `AccountInfo<'info>` with no owner check.
- **Attack Tree:**
  ```
  External caller -> passes arbitrary bytes as token account
    └─ SplTokenAccount::unpack interprets raw bytes as token account struct
      └─ token.amount reported to the message is attacker-controlled
  ```
- **Impact:** Similar to Finding #9 — the unpacked `SplTokenAccount` data is entirely attacker-controlled. While this handler only logs the amount, in any real handler making a decision based on `token.amount` or `token.owner`, the attacker can spoof any value.
- **Exploit:** Pass crafted bytes that deserialize to `SplTokenAccount { amount: 1_000_000, owner: ..., ... }` — program reads attacker-controlled balance.
- **Remediation:** Use `Account<'info, TokenAccount>` or check `ctx.accounts.token.owner == spl_token::ID`.

### 11. Duplicate Mutable Accounts — same account passed twice, no distinctness check
- **File:** `6-duplicate-mutable-accounts/insecure/src/lib.rs:21-22`
- **Evidence:**
  ```rust
  #[derive(Accounts)]
  pub struct Update<'info> {
      user_a: Account<'info, User>,
      user_b: Account<'info, User>,
  }
  ```
  No `constraint = user_a.key() != user_b.key()` guard. Both accounts are mutable.
- **Attack Tree:**
  ```
  External caller -> passes the same pubkey as both user_a and user_b
    └─ Anchor deserializes same account twice as &mut refs
      └─ Writes user_a.data = a, then user_b.data = b — second write overwrites first
        └─ If the protocol expects distinct accounts, this breaks invariants
  ```
- **Impact:** Passing the same account as both `user_a` and `user_b` causes the second write to silently overwrite the first. In financial contracts, this can break balance invariants — e.g., transferring from an account to itself with both fields the same, or writing two different values where the protocol expects both to persist. The exact damage depends on the business logic surrounding the writes.
- **Exploit:** Call `update(user_a = same_acct, user_b = same_acct, a = 100, b = 200)` — `user.data` ends as 200 instead of the protocol expecting two distinct accounts.
- **Remediation:** Add a distinctness constraint:
  ```rust
  #[derive(Accounts)]
  pub struct Update<'info> {
      #[account(constraint = user_a.key() != user_b.key())]
      user_a: Account<'info, User>,
      user_b: Account<'info, User>,
  }
  ```

---

## MEDIUM

### 12. Close without Discriminator — data zeroed but closed_account_discriminator not written
- **File:** `9-closing-accounts/insecure-still/src/lib.rs:20-25`
- **Evidence:**
  ```rust
  for byte in data.deref_mut().iter_mut() {
      *byte = 0;
  }
  // Missing: write CLOSED_ACCOUNT_DISCRIMINATOR
  ```
  This variant zeros the data buffer but does NOT write the `CLOSED_ACCOUNT_DISCRIMINATOR` that Anchor's `Account<'info, T>` loader checks to detect closed accounts.
- **Attack Tree:**
  ```
  Account closed (lamports=0, data zeroed, no discriminator written)
    └─ Reinitialized in same or next transaction
      └─ Anchor's Account<Account<'info, T>> loader sees zeroed bytes but no closed discriminator
        └─ May or may not reject depending on Anchor version — state confusion
  ```
- **Impact:** Without the `CLOSED_ACCOUNT_DISCRIMINATOR`, a reinitialized account may fail to be detected as previously closed by downstream Anchor code, potentially reopening an account that should be dead. Less severe than the variant with no data wipe (Finding #7) but still a state-management flaw.
- **Remediation:** Write `CLOSED_ACCOUNT_DISCRIMINATOR` after zeroing:
  ```rust
  cursor.write_all(&anchor_lang::__private::CLOSED_ACCOUNT_DISCRIMINATOR).unwrap();
  ```

---

## Checked and Cleared

- `9-closing-accounts/insecure-still-still/src/lib.rs` — This variant writes the `CLOSED_ACCOUNT_DISCRIMINATOR` after zeroing data (lines 26-30), matching the secure pattern. It still lacks the `force_defund` handler from the secure variant but the core close is correctly implemented. Not a security finding on its own.
- `0-signer-authorization/secure/src/lib.rs` — Uses `Signer<'info>`. Cleared.
- `3-type-cosplay/secure/src/lib.rs` — Discriminant check present. Cleared.
- `5-arbitrary-cpi/secure/src/lib.rs` — Program ID validated. Cleared.
- `4-initialization/secure/src/lib.rs` — Initialization guard present. Cleared.
- `8-pda-sharing/secure/src/lib.rs` — `withdraw_destination` in PDA seeds. Cleared.
- `7-bump-seed-canonicalization/secure/src/lib.rs` — `find_program_address` used. Cleared.
- `9-closing-accounts/secure/src/lib.rs` — Full close pattern with discriminator + `force_defund`. Cleared.
- `10-sysvar-address-checking/secure/src/lib.rs` — Sysvar ID validated. Cleared.
- `2-owner-checks/secure/src/lib.rs` — SPL token program ID check present. Cleared.
- `1-account-data-matching/secure/src/lib.rs` — SPL token program ID check present. Cleared.
- `6-duplicate-mutable-accounts/secure/src/lib.rs` — `constraint = user_a.key() != user_b.key()` present. Cleared.
- All `recommended/` variants — contain improvements but are not the primary insecure targets. Cleared.

---

## Dependencies

No vulnerable dependencies per OSV. 35 Cargo.toml files scanned across `anchor-lang` 0.20.0–0.25.0, `anchor-spl` 0.20.1, `spl-token` 3.1.1–3.2.0 — no CVEs or security advisories on record. 

**Note:** Anchor 0.20.x is from ~2022 and significantly outdated (current is 0.30+). No known CVEs, but the old versions lack many framework-level safety improvements added since. This is maintenance, not a vulnerability finding.

---

## Remediation Summary

### Immediate (CRITICAL)
1. `0-signer-authorization/insecure/src/lib.rs:16` — Change `AccountInfo` to `Signer` for authority
2. `3-type-cosplay/insecure/src/lib.rs:11` — Use `Account<'info, User>` or add discriminant check before deserialization
3. `5-arbitrary-cpi/insecure/src/lib.rs:11-13` — Validate `token_program.key` against `spl_token::ID` or use `Program<'info, Token>`
4. `4-initialization/insecure/src/lib.rs:12` — Add reinitialization guard (check initialized flag or use `#[account(zero)]`)
5. `8-pda-sharing/insecure/src/lib.rs:12` — Use `withdraw_destination` in PDA seeds instead of `mint`
6. `7-bump-seed-canonicalization/insecure/src/lib.rs:11` — Use `find_program_address` to verify canonical bump, or use Anchor's `[account(seeds = [...], bump)]`

### Short-term (HIGH)
7. `9-closing-accounts/insecure/src/lib.rs:15` — Use `#[account(close = destination)]` or zero data + write `CLOSED_ACCOUNT_DISCRIMINATOR`
8. `10-sysvar-address-checking/insecure/src/lib.rs:17` — Validate sysvar address or use `Sysvar::get()`
9. `2-owner-checks/insecure/src/lib.rs:12` — Check `token.owner == spl_token::ID` before unpack, or use `Account<'info, TokenAccount>`
10. `1-account-data-matching/insecure/src/lib.rs:12` — Same as #9: validate SPL token program ownership
11. `6-duplicate-mutable-accounts/insecure/src/lib.rs:21-22` — Add `constraint = user_a.key() != user_b.key()`

### Short-term (MEDIUM)
12. `9-closing-accounts/insecure-still/src/lib.rs:24` — Write `CLOSED_ACCOUNT_DISCRIMINATOR` after zeroing data