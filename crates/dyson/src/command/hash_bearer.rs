// ===========================================================================
// `dyson hash-bearer <plaintext>` — emit an Argon2id PHC string operators
// can paste into the `auth.hash` field of an HTTP controller config.
//
// Plaintext bearer tokens never go in dyson.json: a leaked config (cloud
// snapshot, accidentally-committed dotfile, scrollback in a terminal
// recording) immediately becomes a valid credential.  Hashing means a
// disclosed config still doesn't grant entry — an attacker would need to
// brute-force Argon2id, which is the whole point of using a memory-hard
// password hash for a static credential.
//
// We default to argon2's library defaults so the parameters travel with
// the hash itself (the PHC string encodes m / t / p), and a future
// upgrade only needs to bump the encoded params, not migrate every
// stored hash.
// ===========================================================================

use dyson::auth::HashedBearerAuth;
use dyson::error::Result;

pub fn run(plaintext: String) -> Result<()> {
    if plaintext.is_empty() {
        return Err(dyson::error::DysonError::Config(
            "hash-bearer: refuse to hash an empty token".into(),
        ));
    }
    let phc = HashedBearerAuth::hash(&plaintext)?;
    println!("{phc}");
    Ok(())
}
