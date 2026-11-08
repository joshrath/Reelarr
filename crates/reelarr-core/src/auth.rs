//! Replaceable, synchronous auth boundary. Run password operations on a blocking pool.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, SystemTime},
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand_core::{OsRng, RngCore};
use reelarr_types::UserInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    AlreadyConfigured,
    InvalidCredentials,
    InvalidInput,
    Unavailable,
}

#[derive(Clone)]
pub struct Session {
    pub token: String,
    pub csrf_token: String,
    pub user: Option<UserInfo>,
    pub expires_at: SystemTime,
}

pub trait AuthService: Send + Sync {
    fn needs_setup(&self) -> Result<bool, AuthError>;
    fn create_admin(&self, username: &str, password: &str) -> Result<UserInfo, AuthError>;
    fn verify_login(&self, username: &str, password: &str) -> Result<UserInfo, AuthError>;
    /// `None` creates a pre-auth session for setup/login CSRF protection.
    fn create_session(&self, user: Option<UserInfo>) -> Result<Session, AuthError>;
    fn lookup_session(&self, token: &str) -> Result<Option<Session>, AuthError>;
    fn revoke_session(&self, token: &str) -> Result<(), AuthError>;
}

struct Admin {
    user: UserInfo,
    password_hash: String,
}
#[derive(Default)]
struct Store {
    admin: Option<Admin>,
    sessions: HashMap<String, Session>,
}

/// Ephemeral single-admin implementation: restarting loses accounts and sessions.
#[derive(Default)]
pub struct InMemoryAuthService {
    store: Mutex<Store>,
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl AuthService for InMemoryAuthService {
    fn needs_setup(&self) -> Result<bool, AuthError> {
        Ok(self
            .store
            .lock()
            .map_err(|_| AuthError::Unavailable)?
            .admin
            .is_none())
    }
    fn create_admin(&self, username: &str, password: &str) -> Result<UserInfo, AuthError> {
        if username.trim().is_empty()
            || username.len() > 128
            || password.len() < 12
            || password.len() > 1024
        {
            return Err(AuthError::InvalidInput);
        }
        if !self.needs_setup()? {
            return Err(AuthError::AlreadyConfigured);
        }
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|_| AuthError::Unavailable)?
            .to_string();
        let mut store = self.store.lock().map_err(|_| AuthError::Unavailable)?;
        // Check again after hashing: concurrent setup must never replace the admin.
        if store.admin.is_some() {
            return Err(AuthError::AlreadyConfigured);
        }
        let user = UserInfo {
            id: random_token(),
            username: username.to_owned(),
        };
        store.admin = Some(Admin {
            user: user.clone(),
            password_hash,
        });
        Ok(user)
    }
    fn verify_login(&self, username: &str, password: &str) -> Result<UserInfo, AuthError> {
        if username.len() > 128 || password.len() > 1024 {
            return Err(AuthError::InvalidCredentials);
        }
        let (user, hash) = {
            let store = self.store.lock().map_err(|_| AuthError::Unavailable)?;
            let admin = store.admin.as_ref().ok_or(AuthError::InvalidCredentials)?;
            (admin.user.clone(), admin.password_hash.clone())
        };
        let hash = PasswordHash::new(&hash).map_err(|_| AuthError::Unavailable)?;
        let valid = Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok();
        if !valid || user.username != username {
            return Err(AuthError::InvalidCredentials);
        }
        Ok(user)
    }
    fn create_session(&self, user: Option<UserInfo>) -> Result<Session, AuthError> {
        let session = Session {
            token: random_token(),
            csrf_token: random_token(),
            user,
            expires_at: SystemTime::now() + Duration::from_secs(86400),
        };
        let mut store = self.store.lock().map_err(|_| AuthError::Unavailable)?;
        store
            .sessions
            .retain(|_, s| s.expires_at > SystemTime::now());
        store
            .sessions
            .insert(session.token.clone(), session.clone());
        Ok(session)
    }
    fn lookup_session(&self, token: &str) -> Result<Option<Session>, AuthError> {
        let mut store = self.store.lock().map_err(|_| AuthError::Unavailable)?;
        if store
            .sessions
            .get(token)
            .is_some_and(|s| s.expires_at <= SystemTime::now())
        {
            store.sessions.remove(token);
        }
        Ok(store.sessions.get(token).cloned())
    }
    fn revoke_session(&self, token: &str) -> Result<(), AuthError> {
        self.store
            .lock()
            .map_err(|_| AuthError::Unavailable)?
            .sessions
            .remove(token);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_and_invalid_sessions_are_rejected() {
        let auth = InMemoryAuthService::default();
        assert_eq!(
            auth.create_admin(" ", "a-long-password"),
            Err(AuthError::InvalidInput)
        );
        assert_eq!(
            auth.create_admin("admin", "short"),
            Err(AuthError::InvalidInput)
        );
        let session = auth.create_session(None).unwrap();
        auth.store
            .lock()
            .unwrap()
            .sessions
            .get_mut(&session.token)
            .unwrap()
            .expires_at = SystemTime::now() - Duration::from_secs(1);
        assert!(auth.lookup_session(&session.token).unwrap().is_none());
        assert!(auth.lookup_session("unknown").unwrap().is_none());
    }
    #[test]
    fn single_admin_hash_and_revocation() {
        let auth = InMemoryAuthService::default();
        let user = auth.create_admin("admin", "a-long-password").unwrap();
        assert_eq!(
            auth.create_admin("other", "a-long-password"),
            Err(AuthError::AlreadyConfigured)
        );
        assert_eq!(
            auth.verify_login("admin", "wrong"),
            Err(AuthError::InvalidCredentials)
        );
        assert_eq!(auth.verify_login("admin", "a-long-password").unwrap(), user);
        let store = auth.store.lock().unwrap();
        assert!(
            store
                .admin
                .as_ref()
                .unwrap()
                .password_hash
                .starts_with("$argon2id$")
        );
        drop(store);
        let session = auth.create_session(Some(user)).unwrap();
        assert!(auth.lookup_session(&session.token).unwrap().is_some());
        auth.revoke_session(&session.token).unwrap();
        assert!(auth.lookup_session(&session.token).unwrap().is_none());
    }
}
