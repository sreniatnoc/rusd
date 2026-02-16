//! Authentication and RBAC (Role-Based Access Control).
//!
//! This module implements etcd-compatible authentication and authorization with:
//! - User and role management
//! - Permission-based access control
//! - JWT token generation and verification
//! - Password hashing with bcrypt
//! - Persistent storage via backend

use bcrypt::{hash, verify};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info};

/// Auth-related errors.
#[derive(Error, Debug)]
pub enum AuthError {
    #[error("User not found: {0}")]
    UserNotFound(String),

    #[error("Role not found: {0}")]
    RoleNotFound(String),

    #[error("User already exists: {0}")]
    UserAlreadyExists(String),

    #[error("Role already exists: {0}")]
    RoleAlreadyExists(String),

    #[error("Invalid credentials")]
    InvalidCredentials,

    #[error("Invalid token")]
    InvalidToken,

    #[error("Permission denied")]
    PermissionDenied,

    #[error("Auth is not enabled")]
    AuthDisabled,

    #[error("Auth is already enabled")]
    AuthAlreadyEnabled,

    #[error("Invalid password: {0}")]
    InvalidPassword(String),

    #[error("JWT error: {0}")]
    JwtError(String),

    #[error("Bcrypt error")]
    BcryptError,

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type AuthResult<T> = Result<T, AuthError>;

/// Permission type for RBAC.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PermissionType {
    Read = 0,
    Write = 1,
    ReadWrite = 2,
}

impl PermissionType {
    pub fn allows_read(&self) -> bool {
        matches!(self, PermissionType::Read | PermissionType::ReadWrite)
    }

    pub fn allows_write(&self) -> bool {
        matches!(self, PermissionType::Write | PermissionType::ReadWrite)
    }
}

/// A single permission within a role.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Permission {
    pub perm_type: PermissionType,
    pub key: Vec<u8>,
    pub range_end: Vec<u8>,
}

impl Permission {
    /// Check if this permission covers the given key range.
    pub fn covers(&self, key: &[u8], range_end: &[u8]) -> bool {
        // Permission covers if the requested range is fully contained
        if key < &self.key[..] {
            return false;
        }

        if self.range_end.is_empty() {
            // Wildcard: only covers exact key match
            key == &self.key[..]
        } else if &self.range_end[..] == b"\0" {
            // Wildcard: covers all keys >= self.key
            true
        } else if range_end.is_empty() {
            // Requested key is checked against permission range
            key < &self.range_end[..]
        } else {
            // Full range check
            key >= &self.key[..] && range_end <= &self.range_end[..]
        }
    }
}

/// A role with associated permissions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Role {
    pub name: String,
    pub permissions: Vec<Permission>,
}

impl Role {
    pub fn new(name: String) -> Self {
        Role {
            name,
            permissions: vec![],
        }
    }

    /// Check if this role grants the specified permission.
    pub fn check_permission(
        &self,
        key: &[u8],
        range_end: &[u8],
        perm_type: PermissionType,
    ) -> bool {
        self.permissions.iter().any(|p| {
            if !p.perm_type.allows_read() && !p.perm_type.allows_write() {
                return false;
            }

            match perm_type {
                PermissionType::Read => p.perm_type.allows_read() && p.covers(key, range_end),
                PermissionType::Write => p.perm_type.allows_write() && p.covers(key, range_end),
                PermissionType::ReadWrite => {
                    (p.perm_type.allows_read() && p.perm_type.allows_write())
                        && p.covers(key, range_end)
                }
            }
        })
    }
}

/// A user with associated roles.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub password_hash: String,
    pub roles: Vec<String>,
}

impl User {
    pub fn new(name: String, password_hash: String) -> Self {
        User {
            name,
            password_hash,
            roles: vec![],
        }
    }
}

/// JWT claims for authentication tokens.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct Claims {
    sub: String,
    exp: usize,
}

/// Authentication and authorization manager.
pub struct AuthStore {
    /// Users indexed by name
    users: RwLock<HashMap<String, User>>,

    /// Roles indexed by name
    roles: RwLock<HashMap<String, Role>>,

    /// Whether authentication is enabled
    enabled: std::sync::atomic::AtomicBool,

    /// JWT signing secret
    jwt_secret: Vec<u8>,

    /// Token TTL
    token_ttl: std::time::Duration,

    /// Revision counter for detecting changes
    revision: std::sync::atomic::AtomicI64,
}

impl AuthStore {
    /// Creates a new AuthStore.
    pub fn new(jwt_secret: Option<String>) -> Self {
        let secret = jwt_secret.unwrap_or_else(|| {
            use base64::Engine;
            use rand::Rng;
            let bytes: [u8; 32] = rand::thread_rng().gen();
            base64::engine::general_purpose::STANDARD.encode(bytes)
        });

        let mut store = AuthStore {
            users: RwLock::new(HashMap::new()),
            roles: RwLock::new(HashMap::new()),
            enabled: std::sync::atomic::AtomicBool::new(false),
            jwt_secret: secret.into_bytes(),
            token_ttl: std::time::Duration::from_secs(3600), // 1 hour default
            revision: std::sync::atomic::AtomicI64::new(1),
        };

        // Create default root user and role
        store.setup_defaults();

        store
    }

    /// Sets up the default root user and role.
    fn setup_defaults(&mut self) {
        let mut users = self.users.write();
        let mut roles = self.roles.write();

        // Create root role with full access
        let mut root_role = Role::new("root".to_string());
        root_role.permissions.push(Permission {
            perm_type: PermissionType::ReadWrite,
            key: vec![],
            range_end: b"\0".to_vec(), // Wildcard: all keys
        });
        roles.insert("root".to_string(), root_role);

        // Create root user (will be set with password via user_change_password)
        let root_user = User::new("root".to_string(), String::new());
        users.insert("root".to_string(), root_user);
    }

    /// Enables authentication.
    pub fn enable(&self) -> AuthResult<()> {
        if self.enabled.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AuthError::AuthAlreadyEnabled);
        }
        self.enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        info!("Authentication enabled");
        Ok(())
    }

    /// Disables authentication.
    pub fn disable(&self) -> AuthResult<()> {
        self.enabled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        info!("Authentication disabled");
        Ok(())
    }

    /// Returns whether authentication is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Authenticates a user and returns a JWT token.
    pub fn authenticate(&self, username: &str, password: &str) -> AuthResult<String> {
        let users = self.users.read();

        let user = users
            .get(username)
            .ok_or(AuthError::UserNotFound(username.to_string()))?;

        // Verify password
        verify(password, &user.password_hash).map_err(|_| AuthError::InvalidCredentials)?;

        drop(users);

        // Generate JWT token
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as usize;

        let claims = Claims {
            sub: username.to_string(),
            exp: now + self.token_ttl.as_secs() as usize,
        };

        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(&self.jwt_secret),
        )
        .map_err(|e| AuthError::JwtError(e.to_string()))?;

        debug!(username, "User authenticated");
        Ok(token)
    }

    /// Verifies a JWT token and returns the username.
    pub fn verify_token(&self, token: &str) -> AuthResult<String> {
        let validation = Validation::default();
        let data = decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.jwt_secret),
            &validation,
        )
        .map_err(|_| AuthError::InvalidToken)?;

        Ok(data.claims.sub)
    }

    /// Checks if a user has permission to access a key range.
    pub fn check_permission(
        &self,
        username: &str,
        key: &[u8],
        range_end: &[u8],
        perm_type: PermissionType,
    ) -> AuthResult<bool> {
        // Root user has all permissions
        if username == "root" {
            return Ok(true);
        }

        let users = self.users.read();
        let user = users
            .get(username)
            .ok_or(AuthError::UserNotFound(username.to_string()))?;

        let roles = self.roles.read();

        // Check if any of the user's roles grant this permission
        for role_name in &user.roles {
            if let Some(role) = roles.get(role_name) {
                if role.check_permission(key, range_end, perm_type.clone()) {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    /// Adds a new user.
    pub fn user_add(&self, name: &str, password: &str, no_password: bool) -> AuthResult<()> {
        let mut users = self.users.write();

        if users.contains_key(name) {
            return Err(AuthError::UserAlreadyExists(name.to_string()));
        }

        let password_hash = if no_password {
            String::new()
        } else {
            hash(password, 10).map_err(|_| AuthError::BcryptError)?
        };

        users.insert(name.to_string(), User::new(name.to_string(), password_hash));

        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        debug!(name, "User added");
        Ok(())
    }

    /// Gets a user.
    pub fn user_get(&self, name: &str) -> AuthResult<User> {
        self.users
            .read()
            .get(name)
            .cloned()
            .ok_or(AuthError::UserNotFound(name.to_string()))
    }

    /// Lists all usernames.
    pub fn user_list(&self) -> Vec<String> {
        self.users.read().keys().cloned().collect()
    }

    /// Deletes a user.
    pub fn user_delete(&self, name: &str) -> AuthResult<()> {
        if name == "root" {
            return Err(AuthError::Internal("Cannot delete root user".to_string()));
        }

        let mut users = self.users.write();
        users
            .remove(name)
            .ok_or(AuthError::UserNotFound(name.to_string()))?;

        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        debug!(name, "User deleted");
        Ok(())
    }

    /// Changes a user's password.
    pub fn user_change_password(&self, name: &str, password: &str) -> AuthResult<()> {
        if password.is_empty() {
            return Err(AuthError::InvalidPassword(
                "Password cannot be empty".to_string(),
            ));
        }

        let mut users = self.users.write();

        if let Some(user) = users.get_mut(name) {
            user.password_hash = hash(password, 10).map_err(|_| AuthError::BcryptError)?;

            self.revision
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            debug!(name, "Password changed");
            Ok(())
        } else {
            Err(AuthError::UserNotFound(name.to_string()))
        }
    }

    /// Grants a role to a user.
    pub fn user_grant_role(&self, username: &str, role: &str) -> AuthResult<()> {
        // Check that role exists
        if !self.roles.read().contains_key(role) {
            return Err(AuthError::RoleNotFound(role.to_string()));
        }

        let mut users = self.users.write();

        if let Some(user) = users.get_mut(username) {
            if !user.roles.contains(&role.to_string()) {
                user.roles.push(role.to_string());
                self.revision
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                debug!(username, role, "Role granted to user");
            }
            Ok(())
        } else {
            Err(AuthError::UserNotFound(username.to_string()))
        }
    }

    /// Revokes a role from a user.
    pub fn user_revoke_role(&self, username: &str, role: &str) -> AuthResult<()> {
        let mut users = self.users.write();

        if let Some(user) = users.get_mut(username) {
            user.roles.retain(|r| r != role);
            self.revision
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            debug!(username, role, "Role revoked from user");
            Ok(())
        } else {
            Err(AuthError::UserNotFound(username.to_string()))
        }
    }

    /// Adds a new role.
    pub fn role_add(&self, name: &str) -> AuthResult<()> {
        let mut roles = self.roles.write();

        if roles.contains_key(name) {
            return Err(AuthError::RoleAlreadyExists(name.to_string()));
        }

        roles.insert(name.to_string(), Role::new(name.to_string()));

        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        debug!(name, "Role added");
        Ok(())
    }

    /// Gets a role.
    pub fn role_get(&self, name: &str) -> AuthResult<Role> {
        self.roles
            .read()
            .get(name)
            .cloned()
            .ok_or(AuthError::RoleNotFound(name.to_string()))
    }

    /// Lists all role names.
    pub fn role_list(&self) -> Vec<String> {
        self.roles.read().keys().cloned().collect()
    }

    /// Deletes a role.
    pub fn role_delete(&self, name: &str) -> AuthResult<()> {
        if name == "root" {
            return Err(AuthError::Internal("Cannot delete root role".to_string()));
        }

        let mut roles = self.roles.write();
        roles
            .remove(name)
            .ok_or(AuthError::RoleNotFound(name.to_string()))?;

        // Remove role from all users
        let mut users = self.users.write();
        for user in users.values_mut() {
            user.roles.retain(|r| r != name);
        }

        self.revision
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        debug!(name, "Role deleted");
        Ok(())
    }

    /// Grants a permission to a role.
    pub fn role_grant_permission(&self, role: &str, perm: Permission) -> AuthResult<()> {
        let mut roles = self.roles.write();

        if let Some(role_obj) = roles.get_mut(role) {
            // Avoid duplicates
            if !role_obj.permissions.iter().any(|p| {
                p.key == perm.key && p.range_end == perm.range_end && p.perm_type == perm.perm_type
            }) {
                role_obj.permissions.push(perm);
                self.revision
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                debug!(role, "Permission granted to role");
            }
            Ok(())
        } else {
            Err(AuthError::RoleNotFound(role.to_string()))
        }
    }

    /// Revokes a permission from a role.
    pub fn role_revoke_permission(
        &self,
        role: &str,
        key: &[u8],
        range_end: &[u8],
    ) -> AuthResult<()> {
        let mut roles = self.roles.write();

        if let Some(role_obj) = roles.get_mut(role) {
            let original_len = role_obj.permissions.len();
            role_obj
                .permissions
                .retain(|p| !(p.key == key && p.range_end == range_end));

            if role_obj.permissions.len() != original_len {
                self.revision
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                debug!(role, "Permission revoked from role");
            }
            Ok(())
        } else {
            Err(AuthError::RoleNotFound(role.to_string()))
        }
    }

    /// Gets the current revision.
    pub fn get_revision(&self) -> i64 {
        self.revision.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_covers() {
        let perm = Permission {
            perm_type: PermissionType::Read,
            key: b"foo".to_vec(),
            range_end: b"fop".to_vec(),
        };

        assert!(perm.covers(b"foo", b""));
        assert!(perm.covers(b"foobar", b""));
        assert!(!perm.covers(b"fop", b""));
        assert!(!perm.covers(b"fon", b"")); // "fon" < "foo", so outside range
    }

    #[test]
    fn test_user_and_role() {
        let store = AuthStore::new(None);

        store.role_add("editor").unwrap();
        store
            .user_add("alice", "test-only-not-a-secret", false)
            .unwrap();
        store.user_grant_role("alice", "editor").unwrap();

        let user = store.user_get("alice").unwrap();
        assert!(user.roles.contains(&"editor".to_string()));
    }

    #[test]
    fn test_permission_check() {
        let store = AuthStore::new(None);

        store.role_add("reader").unwrap();
        let perm = Permission {
            perm_type: PermissionType::Read,
            key: b"keys/".to_vec(),
            range_end: b"keys0".to_vec(),
        };

        store.role_grant_permission("reader", perm).unwrap();
        store
            .user_add("bob", "test-only-not-a-secret", false)
            .unwrap();
        store.user_grant_role("bob", "reader").unwrap();

        let allowed = store
            .check_permission("bob", b"keys/app1", b"", PermissionType::Read)
            .unwrap();
        assert!(allowed);

        let denied = store
            .check_permission("bob", b"keys/app1", b"", PermissionType::Write)
            .unwrap();
        assert!(!denied);
    }

    #[test]
    fn test_root_user() {
        let store = AuthStore::new(None);

        let allowed = store
            .check_permission("root", b"anything", b"", PermissionType::ReadWrite)
            .unwrap();
        assert!(allowed);
    }
}
