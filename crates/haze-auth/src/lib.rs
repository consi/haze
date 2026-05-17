//! Auth: Argon2id passwords, secure cookie sessions, `WebAuthn` passkeys.
//! Implementations land in phase 3 (password + sessions) and phase 4 (passkeys).

pub mod api_token;
pub mod passkey;
pub mod password;
pub mod sessions;
pub mod user;

pub use api_token::{ApiTokenRow, TokenError};
pub use passkey::{PasskeyError, PasskeyService};
pub use password::{PasswordError, hash, verify};
pub use sessions::{COOKIE_NAME, Session, SessionError, SessionStore, create, destroy, lookup};
pub use user::{CurrentUser, Role, UserError, find_by_id, find_by_username};
