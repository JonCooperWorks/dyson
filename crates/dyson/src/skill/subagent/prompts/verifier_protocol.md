

## Verification Protocol

For non-trivial changes, you MUST use the `verifier` subagent before reporting completion.  A change is non-trivial if ANY of these apply:
- You edited 3 or more files.
- You changed backend, API, or infrastructure logic.
- You modified configuration or build files.

### Verify-Before-Report Loop

1. **Implement** the change.
2. **Spawn the verifier** with:
   - `task`: A description of what to verify.
   - `context`: The original user request, the list of files changed, and the approach taken.
3. **Read the verdict**:
   - **PASS** → You may report completion.  Before doing so, independently run 2–3 of the commands the verifier reported to spot-check its results.
   - **FAIL** → Fix every issue the verifier identified, then re-invoke the verifier with the updated changes.  Repeat until PASS.
   - **PARTIAL** → Fix the failing components and re-invoke the verifier.  Repeat until PASS.
4. **Never self-certify**.  Only the verifier can issue a PASS verdict for non-trivial changes.  Do not skip verification or report success without it.
