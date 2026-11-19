//! Buffers for the credentials file and the token: zeroed before their memory is freed, never
//! printed (a redacting `Debug`, no `Display`).

use std::fmt;
use std::sync::atomic::{Ordering, compiler_fence};

/// Bytes that must not outlive their use. The allocation never grows: `push` refuses what does
/// not fit, since growing would leave a copy behind in freed memory.
pub struct Secret(Vec<u8>);

impl Secret {
    /// A zero-filled buffer of exactly `len` bytes.
    pub fn zeroed(len: usize) -> Secret {
        Secret(vec![0; len])
    }

    /// An empty buffer that holds up to `cap` bytes.
    pub fn with_capacity(cap: usize) -> Secret {
        Secret(Vec::with_capacity(cap))
    }

    /// Appends `bytes`; false, and nothing appended, when they do not fit.
    #[must_use]
    pub fn push(&mut self, bytes: &[u8]) -> bool {
        if self.0.capacity() - self.0.len() < bytes.len() {
            return false;
        }
        self.0.extend_from_slice(bytes);
        true
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // The whole allocation, spare capacity included; volatile so that it is not optimised
        // away as a dead store.
        let cap = self.0.capacity();
        let p = self.0.as_mut_ptr();
        for i in 0..cap {
            // SAFETY: i < capacity, inside the allocation this Vec owns; u8 has no invalid values.
            unsafe { std::ptr::write_volatile(p.add(i), 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_never_grows() {
        let mut s = Secret::with_capacity(4);
        assert!(s.push(b"ab"));
        assert!(s.push(b"cd"));
        assert!(!s.push(b"e"));
        assert_eq!(s.as_bytes(), b"abcd");
    }

    #[test]
    fn debug_is_redacted() {
        let mut s = Secret::with_capacity(8);
        assert!(s.push(b"SECRET-X"));
        let shown = format!("{s:?}"); // lint-allow: debug (the redaction under test)
        assert_eq!(shown, "Secret([redacted])");
    }
}
