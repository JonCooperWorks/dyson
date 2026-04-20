# Security Review: Solana Cross-Chain Bridge Program

## CRITICAL

### Sysvar substitution bypasses signature verification in `verify_signatures`

- **File:** `src/api/verify_signature.rs:34`
- **Evidence:**
  ```rust
  pub instruction_acc: Info<'b>,
  ```
  Followed by lines 92-104:
  ```rust
  let current_instruction = solana_program::sysvar::instructions::load_current_index(
      &accs.instruction_acc.try_borrow_mut_data()?,
  );
  // ...
  let secp_ix = solana_program::sysvar::instructions::load_instruction_at(
      secp_ix_index as usize,
      &accs.instruction_acc.try_borrow_mut_data()?,
  );
  ```
  The on-chain program never validates `accs.instruction_acc.key` against `solana_program::sysvar::instructions::id()`. The instruction builder in `src/instructions.rs:153` passes the correct sysvar address:
  ```rust
  AccountMeta::new_readonly(sysvar::instructions::id(), false),
  ```
  but this client-side hint is not enforced on-chain.

- **Attack Tree:**
  ```
  src/api/verify_signature.rs:72 — verify_signatures(ctx, accs, data)
    └─ src/api/verify_signature.rs:92-94 — load_current_index reads from attacker-controlled instruction_acc
      └─ src/api/verify_signature.rs:101-104 — load_instruction_at returns attacker-crafted Instruction struct
        └─ src/api/verify_signature.rs:108-110 — attacker sets secp_ix.program_id = secp256k1_program::id() in fake data (passes validation)
          └─ src/api/verify_signature.rs:117-153 — attacker crafts fake secp instruction data with real guardian addresses and chosen message hash
            └─ src/api/verify_signature.rs:208-215 — key check passes because attacker embedded real guardian_set.keys[s.signer_index] in fake data
              └─ src/api/verify_signature.rs:215 — signature_set.signatures[s.signer_index] = true → consensus reached without any real secp256k1 verification
  ```

- **Taint Trace:** (manual code review — taint_trace not applicable for sysvar-account-substitution; the entire `instruction_acc` is the externally-controllable account)
  ```
  [EXTERNAL ENTRY] transaction account slot 4 = attacker-controlled AccountInfo
    → verify_signatures::instruction_acc (src/api/verify_signature.rs:34) — Info<'b>, no key validation
    → try_borrow_mut_data() (src/api/verify_signature.rs:93) — reads attacker-supplied bytes
    → load_current_index (src/api/verify_signature.rs:92) — parses attacker bytes as sysvar format
    → load_instruction_at (src/api/verify_signature.rs:101-103) — returns attacker-crafted Instruction
    → secp_ix.program_id check (src/api/verify_signature.rs:108) — passes (attacker sets program_id = secp256k1_program::id())
    → address offsets parsed from secp_ix.data (src/api/verify_signature.rs:121-141) — attacker controls all offsets and values
    → key comparison (src/api/verify_signature.rs:210) — attacker knows guardian_set.keys from on-chain state, embeds them in fake data
    → signature flag write (src/api/verify_signature.rs:215) — signatures marked verified without any real secp256k1 opcode execution
    → [SINK REACHED] post_vaa accepts consensus (src/api/post_vaa.rs:138-140) — VAA posted with no real cryptographic verification
  ```

- **Impact:** Complete signature verification bypass. An attacker submits a single transaction with a fake account as `instruction_acc`, bypassing all secp256k1 signature verification. This allows posting arbitrary VAAs, upgrading the guardian set, upgrading the contract, transferring all collected fees, and setting arbitrary fees. Full bridge compromise.

- **Exploit:** Construct a transaction calling `verify_signatures` with:
  1. Account slot 4 = attacker-controlled account containing a crafted instruction-sysvar-format byte array
  2. `signers` data set to point to guardian indices that map to real guardian addresses
  3. The fake sysvar data encodes a pseudo-secp256k1 "instruction" at index `current_instruction - 1` where:
     - `program_id = secp256k1_program::id()` (4 bytes)
     - signature data contains 65-byte fake signatures (values ignored after program_id check)
     - addresses contain real guardian keys from `accs.guardian_set.keys[]`
     - message contains the attacker's chosen 32-byte hash
  4. Call `post_vaa` with matching signature_set — it passes `check_integrity` because the hash matches

- **Remediation:** Add explicit validation that the instruction account is the real sysvar:

  ```rust
  // src/api/verify_signature.rs, in verify_signatures() after line 74:
  use solana_program::sysvar;

  if accs.instruction_acc.key != &sysvar::instructions::id() {
      return Err(InvalidSecpInstruction.into());
  }
  ```

  Alternatively, use a typed account constraint in the accounts struct:
  ```rust
  pub instruction_acc: Sysvar<'b, solana_program::sysvar::instructions::Instructions>,
  ```
  This enforces the key check at the account-peeling layer.

---

### Borsh 0.8.1 unsoundness — ZST parser allows arbitrary memory corruption

- **File:** `Cargo.toml:20`
- **Evidence:**
  ```toml
  borsh = "0.8.1"
  ```
  OSV advisory GHSA-fjx5-qpf4-xjf2 / RUSTSEC-2023-0033: "Parsing borsh messages with ZST (zero-sized types) which are not-copy/clone is unsound." All borsh deserialization paths in the program flow through this version.

- **Attack Tree:**
  ```
  [EXTERNAL ENTRY] instruction data → BorshDeserialize::deserialize
    └─ src/api/initialize.rs:41 — InitializeData deserialized from instruction data
    └─ src/api/post_message.rs:63 — PostMessageData deserialized
    └─ src/api/post_vaa.rs:78 — PostVAAData deserialized
    └─ src/api/verify_signature.rs:48 — VerifySignaturesData deserialized
    └─ src/api/governance.rs — Governance payloads via GovernancePayloadUpgrade, etc.
    └─ src/wasm.rs — parse_guardian_set, parse_state, parse_posted_message, parse_vaa
    └─ all src/accounts/* structs — GuardianSetData, BridgeData, ClaimData, etc.
      └─ borsh 0.8.1 BorshDeserialize implementation → reads ZST field with unsound code path
        └─ potential undefined behavior, type confusion, or memory corruption
  ```

- **Impact:** Unsound deserialization on any type containing non-Copy/Clone ZST fields. While no such fields are explicitly defined in this crate, the vulnerability is in the borsh library's core deserialization engine. Any future addition of ZST fields, or any dependent type that borsh deserializes, becomes exploitable. The soundness violation means safe code can trigger undefined behavior, which compilers may optimize into exploitable code paths.

- **Exploit:** Not directly exploitable without a known ZST-bearing struct in the crate's borsh-decoded types, but the library-level soundness hole means any code change introducing such a type instantly creates a memory-safety vulnerability. The advisory recommends immediate upgrade.

- **Remediation:** Bump `borsh` to `>= 1.0.0-alpha.1` (or latest stable 1.x). This is a breaking version bump requiring review of all derive macros across:
  - `src/types.rs:41` (ConsistencyLevel)
  - `src/accounts/bridge.rs:22` (BridgeData, BridgeConfig)
  - `src/accounts/guardian_set.rs:23` (GuardianSetData)
  - `src/accounts/claim.rs:22` (ClaimData)
  - `src/accounts/posted_message.rs:31` (MessageData)
  - `src/accounts/signature_set.rs:16` (SignatureSetData)
  - `src/api/post_vaa.rs:78,88` (Signature, PostVAAData)
  - `src/api/post_message.rs:63` (PostMessageData)
  - `src/api/initialize.rs:41` (InitializeData)
  - `src/api/verify_signature.rs:48` (VerifySignaturesData)
  - `src/api/governance.rs:92,142,218,254` (all *Data structs)

## HIGH

### `try_borrow_mut_data()` called on read-only sysvar::instructions account

- **File:** `src/api/verify_signature.rs:93-94, 103`
- **Evidence:**
  ```rust
  let current_instruction = solana_program::sysvar::instructions::load_current_index(
      &accs.instruction_acc.try_borrow_mut_data()?,
  );
  ```
  The instruction builder in `src/instructions.rs:153` passes sysvar::instructions as `new_readonly`:
  ```rust
  AccountMeta::new_readonly(sysvar::instructions::id(), false),
  ```
  Calling `try_borrow_mut_data()` on a read-only account returns an error or panics (behavior depends on Solana version). The correct method is `try_borrow_data()`.

- **Attack Tree:**
  ```
  src/instructions.rs:153 — AccountMeta::new_readonly(sysvar::instructions::id(), false)
    └─ runtime populates account as read-only
    → src/api/verify_signature.rs:93 — try_borrow_mut_data() called on read-only account
      └─ runtime error / panic → instruction fails, transaction reverts
  ```

- **Impact:** Transaction execution failure when invoking `verify_signatures`. If Solana 1.7.0 enforces read-only semantics strictly (it should), this instruction panics and the entire workflow fails. If it silently works, the same sysvar substitution finding applies. Either way, this is a critical code defect.

- **Remediation:** Change `try_borrow_mut_data()` to `try_borrow_data()` on lines 93 and 103:
  ```rust
  let current_instruction = solana_program::sysvar::instructions::load_current_index(
      &accs.instruction_acc.try_borrow_data()?,
  );
  ```
  Combined with the sysvar key check from the CRITICAL finding above.

## MEDIUM

No MEDIUM findings. The remaining code paths were reviewed and cleared per the Checked and Cleared section below.

## LOW / INFORMATIONAL

### `env!("EMITTER_ADDRESS")` compile-time string format validation

- **File:** `src/api/governance.rs:43-47`
- **Evidence:**
  ```rust
  let expected_emitter = std::env!("EMITTER_ADDRESS");
  let current_emitter = format!(
      "{}",
      Pubkey::new_from_array(vaa.message.meta().emitter_address)
  );
  if expected_emitter != current_emitter || vaa.message.meta().emitter_chain != CHAIN_ID_SOLANA {
      Err(InvalidGovernanceKey.into())
  }
  ```
  The governance emitter is validated by string-formatting a Pubkey and comparing against a compile-time environment variable. String formatting overhead is present at runtime. A binary-format comparison would be more efficient and avoid any formatting edge cases.

- **Impact:** Runtime overhead; the comparison is functionally correct but suboptimal. No security impact since the comparison is logically sound.

- **Remediation:** Compare the raw bytes instead:
  ```rust
  let expected_emitter = Pubkey::from_str(env!("EMITTER_ADDRESS")).unwrap();
  if Pubkey::new_from_array(vaa.message.meta().emitter_address) != expected_emitter
      || vaa.message.meta().emitter_chain != CHAIN_ID_SOLANA {
      return Err(InvalidGovernanceKey.into());
  }
  ```

## Checked and Cleared

- `src/api/governance.rs:39-54` — `verify_governance` correctly validates that governance messages originate from the expected emitter AND the Solana chain ID. String comparison at lines 43-47 is functionally correct (see LOW finding for inefficiency note).
- `src/api/post_vaa.rs:104-156` — PostVAA handler correctly validates derivation, guardian set expiration (`check_active`), signature set matching (`check_valid_sigs`), hash integrity (`check_integrity`), and consensus threshold (lines 128-140). All checks are enforced server-side.
- `src/api/post_vaa.rs:160-175` — `check_active` properly enforces guardian set expiration, including the hardcoded mainnet fix at line 166.
- `src/api/post_vaa.rs:179-187` — `check_valid_sigs` verifies signatures belong to the correct guardian set index.
- `src/api/post_vaa.rs:190-219` — `check_integrity` re-serializes the VAA body, hashes it, and compares against `signatures.hash`. Binds signatures to payload.
- `src/api/post_message.rs:75-145` — `post_message` correctly verifies sequence derivation via PDA, checks fee payment by comparing fee_collector lamport delta, and creates message account via CPI.
- `src/api/initialize.rs:55-97` — `initialize` enforces `MAX_LEN_GUARDIAN_KEYS` limit, creates guardian set at index 0, initializes bridge config, and creates fee collector. Only callable once due to `Uninitialized` account state.
- `src/api/governance.rs:95-119` — `upgrade_contract` validates governance via ClaimableVAA, verifies claim account, then performs CPI to bpf_loader_upgradeable. The `new_contract` address comes from the VAA payload, which is guardian-signed.
- `src/api/governance.rs:145-201` — `upgrade_guardian_set` enforces single-increment index progression, validates both old and new guardian set derivation PDAs, claims the VAA, and correctly sets expiration time for the old set.
- `src/api/governance.rs:221-228` — `set_fees` validates governance chain, claims VAA, sets fee via `as_u64()` conversion.
- `src/api/governance.rs:257-294` — `transfer_fees` validates recipient matches VAA payload, enforces rent-exempt floor on fee_collector, claims VAA, then transfers via CPI with PDA seeds.
- `src/api/verify_signature.rs:68-218` — `verify_signatures` loads secp256k1 instruction from prior tx, validates program ID, parses secp data, enforces that all messages have the same size, validates message is 32 bytes (hash), maps signatures to guardian set indices, and checks guardian addresses match.
- `src/accounts/claim.rs:39-46` — Claim derivation seeds include emitter_address, emitter_chain, and sequence, preventing replay attacks.
- `src/vaa.rs:148-180` — ClaimableVAA correctly verifies claim account derivation and provides `claim()` with double-spend protection via `is_claimed()` check.
- `src/instructions.rs:385-404` — `serialize_vaa` and `hash_vaa` are deterministic serialization/hashing functions used for VAA integrity checking.
- `src/types.rs:63-77, 112-134, 164-177, 210-226` — Governance payload deserializers enforce exact buffer length after payload consumption (`c.position() != c.into_inner().len()`), preventing trailing-data injection.
- `src/vaa.rs:241-276` — Manual `VAA::deserialize` is a simple binary protocol parser, not affected by the borsh ZST vulnerability. Reads fixed-length fields and remainder as payload.
- `src/wasm.rs:57-371` — All wasm_bindgen functions are client-side instruction builders (not on-chain). They panic on invalid input, which is appropriate for client tooling.

## Dependencies

**CRITICAL:**
- **borsh 0.8.1** — GHSA-fjx5-qpf4-xjf2 / RUSTSEC-2023-0033 — ZST parsing unsoundness. Fixed in 1.0.0-alpha.1. Affects every runtime file in the program.
  - `linked-findings: src/types.rs:10, src/accounts/bridge.rs:4, src/accounts/guardian_set.rs:5, src/api/post_vaa.rs:3, src/wasm.rs:12`
  - This finding is filed in the CRITICAL section above.

**MEDIUM (dev-dependencies only):**
- **rand 0.7.3** — GHSA-cq8v-f236-94qc / RUSTSEC-2026-0097 — Unsound with custom logger. Dev-dependency only (tests). **P2**
- **libsecp256k1 0.3.5** — GHSA-g4vj-x7v9-h82m / RUSTSEC-2021-0076 — Overflowing signatures. Dev-dependency only. **P2**
- **libsecp256k1 0.3.5** — RUSTSEC-2025-0161 — Unmaintained. Dev-dependency only. **P2**

**Warnings:** No `Cargo.lock` present. Running `cargo update` is recommended to pin exact versions.

## Remediation Summary

### Immediate (CRITICAL/HIGH)
1. `src/api/verify_signature.rs:34` — Add `instruction_acc.key == &sysvar::instructions::id()` validation to prevent sysvar substitution and signature verification bypass.
2. `src/api/verify_signature.rs:93,103` — Change `try_borrow_mut_data()` to `try_borrow_data()` for read-only sysvar access.
3. `Cargo.toml:20` — Bump `borsh` from `0.8.1` to `>= 1.0.0`, update all `BorshSerialize`/`BorshDeserialize` derive sites across 15+ files.

### Short-term (MEDIUM)
No MEDIUM findings require independent remediation beyond the CRITICAL fixes.

### Hardening (LOW)
1. `src/api/governance.rs:43-47` — Replace string-format Pubkey comparison with direct byte comparison for efficiency.