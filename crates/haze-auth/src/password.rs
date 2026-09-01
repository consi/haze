//! Argon2id password hashing. OWASP 2024 parameters: m=19 MiB, t=2, p=1.
//! Hashes are stored as PHC strings so future migrations are graceful.

use argon2::{Algorithm, Argon2, Params, Version};
use password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};

#[derive(Debug, thiserror::Error)]
pub enum PasswordError {
    #[error("hashing failed: {0}")]
    Hash(String),
    #[error("verification failed: {0}")]
    Verify(String),
    #[error("invalid stored hash: {0}")]
    InvalidHash(String),
}

fn argon2() -> Argon2<'static> {
    // m=19 456 KiB (~19 MiB), t=2 iterations, p=1 lane. OWASP 2024.
    let params = Params::new(19_456, 2, 1, None).expect("static params are valid");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash(password: &str) -> Result<String, PasswordError> {
    let hasher = argon2();
    let hash = hasher
        .hash_password(password.as_bytes())
        .map_err(|e| PasswordError::Hash(e.to_string()))?;
    Ok(hash.to_string())
}

pub fn verify(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    let parsed =
        PasswordHash::new(stored_hash).map_err(|e| PasswordError::InvalidHash(e.to_string()))?;
    let hasher = argon2();
    match hasher.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::PasswordInvalid) => Ok(false),
        Err(e) => Err(PasswordError::Verify(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h = hash("hunter2").unwrap();
        assert!(verify("hunter2", &h).unwrap());
        assert!(!verify("hunter3", &h).unwrap());
    }

    #[test]
    fn phc_format() {
        let h = hash("x").unwrap();
        assert!(h.starts_with("$argon2id$"));
    }

    #[test]
    fn rejects_garbage_hash() {
        let err = verify("x", "not a phc string");
        assert!(matches!(err, Err(PasswordError::InvalidHash(_))));
    }
}
