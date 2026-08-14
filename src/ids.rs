//! Secure, unpredictable identifier generation.
//!
//! Share IDs are the *only* thing a network client ever provides to look up
//! a file. They must not be guessable, sequential, or derived from anything
//! predictable (filename, timestamp, counter). We use the OS CSPRNG via
//! `rand::rngs::OsRng` and encode with a URL-safe alphabet.

use rand::rngs::OsRng;
use rand::RngCore;

const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

/// Generates a share ID with ~128 bits of entropy (22 chars from a 58-symbol
/// alphabet gives well over 128 bits: log2(58^22) ≈ 128.9).
pub fn generate_share_id() -> String {
    generate_random_token(22)
}

/// Generates a random session/auth token (used for password-unlock cookies).
pub fn generate_session_token() -> String {
    generate_random_token(32)
}

fn generate_random_token(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_correct_length() {
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            let id = generate_share_id();
            assert_eq!(id.len(), 22);
            assert!(seen.insert(id), "collision detected in 10k samples");
        }
    }

    #[test]
    fn ids_only_use_alphabet_chars() {
        let id = generate_share_id();
        for c in id.chars() {
            assert!(ALPHABET.contains(&(c as u8)));
        }
    }
}
