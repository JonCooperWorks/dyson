// ===========================================================================
// Credential — a zeroize-on-drop wrapper for secret strings.
//
// Replaces raw `String` fields wherever API keys, tokens, or other secrets
// are stored.  Provides:
//   - Automatic zeroing of memory on drop (via the `zeroize` crate)
//   - Explicit construction (no accidental String-to-Credential coercion)
//   - Debug impl that redacts the value
//
// Used by:
//   - BearerTokenAuth (token field)
//   - ApiKeyAuth (key field)
//   - AgentSettings (api_key field → future migration)
//   - Anywhere else a secret string needs to be held in memory
// ===========================================================================

use std::fmt;

use zeroize::Zeroize;

/// A secret string that is zeroed from memory when dropped.
///
/// Use this instead of `String` for API keys, bearer tokens, and other
/// sensitive values.  The `Debug` impl redacts the value to prevent
/// accidental logging.
///
/// ```ignore
/// let cred = Credential::new("sk-ant-secret-key".to_string());
/// assert_eq!(cred.expose(), "sk-ant-secret-key");
/// // prints: Credential("***")
/// println!("{:?}", cred);
/// // memory is zeroed when `cred` goes out of scope
/// ```
pub struct Credential {
    value: String,
}

impl Credential {
    /// Create a new credential from a secret string.
    pub fn new(value: String) -> Self {
        Self { value }
    }

    /// Access the raw secret value.
    ///
    /// Named `expose()` rather than `as_str()` to make secret access
    /// explicit and easy to audit in code review.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Returns true if the credential is empty.
    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Credential").field(&"***").finish()
    }
}

impl Clone for Credential {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
        }
    }
}

impl From<&str> for Credential {
    fn from(s: &str) -> Self {
        Self::new(s.to_string())
    }
}

impl From<String> for Credential {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl PartialEq<&str> for Credential {
    fn eq(&self, other: &&str) -> bool {
        self.value == *other
    }
}

impl PartialEq<str> for Credential {
    fn eq(&self, other: &str) -> bool {
        self.value == other
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expose_returns_value() {
        let cred = Credential::new("secret-123".into());
        assert_eq!(cred.expose(), "secret-123");
    }

    #[test]
    fn debug_redacts() {
        let cred = Credential::new("secret-123".into());
        let debug = format!("{:?}", cred);
        assert!(!debug.contains("secret-123"));
        assert!(debug.contains("***"));
    }

    #[test]
    fn is_empty() {
        assert!(Credential::new(String::new()).is_empty());
        assert!(!Credential::new("x".into()).is_empty());
    }

    #[test]
    fn clone_works() {
        let a = Credential::new("secret".into());
        let b = a.clone();
        assert_eq!(a.expose(), b.expose());
    }

    #[test]
    fn zeroize_on_drop() {
        let cred = Box::new(Credential::new("super-secret-key-for-zeroize-test".into()));
        let raw = Box::into_raw(cred);

        let struct_size = std::mem::size_of::<Credential>();
        let secret_len = "super-secret-key-for-zeroize-test".len();
        let secret_len_bytes = secret_len.to_ne_bytes();

        // Before drop: the String's length field should be present.
        let pre_drop_bytes: Vec<u8> = unsafe {
            std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec()
        };
        let pre_match_count = pre_drop_bytes
            .windows(secret_len_bytes.len())
            .filter(|w| *w == secret_len_bytes)
            .count();
        assert!(pre_match_count > 0, "length field should be present before drop");

        // Drop (triggers zeroize).
        unsafe { std::ptr::drop_in_place(raw); }

        // After drop: the length field should be zeroed.
        let post_drop_bytes: Vec<u8> = unsafe {
            std::slice::from_raw_parts(raw as *const u8, struct_size).to_vec()
        };
        let post_match_count = post_drop_bytes
            .windows(secret_len_bytes.len())
            .filter(|w| *w == secret_len_bytes)
            .count();
        assert!(
            post_match_count < pre_match_count,
            "length field should be zeroed after drop"
        );

        unsafe {
            let layout = std::alloc::Layout::new::<Credential>();
            std::alloc::dealloc(raw as *mut u8, layout);
        }
    }
}
