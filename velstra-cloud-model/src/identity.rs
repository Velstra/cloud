//! Who a person is, and how they prove it.
//!
//! ## The one structural decision
//!
//! A user's **credential is not part of the user**. `UserSpec` carries a display
//! name and whether the account is disabled; the password hash lives in a
//! separate [`Credential`] object, in a collection the API does not serve at
//! all.
//!
//! The tempting alternative is a `password_hash` field on the user, redacted on
//! read. That is one `serde(skip)` away from correct and one forgotten code path
//! away from a disaster: `list`, `get`, `watch`, the console's cache, an
//! operation record, an audit line and a proxy hop are each a place the object
//! travels through, and each would have to remember. A collection the API has
//! never heard of cannot leak through any of them, because there is no route
//! that reaches it.
//!
//! [`Session`] is separated from the user for the same reason and one more: a
//! session is revoked by deleting one small object, and revoking every session
//! must not mean rewriting the user.
//!
//! ## What is stored is never what is presented
//!
//! A password is stored as an Argon2id PHC string — salted, memory-hard,
//! verified in constant time by the `argon2` crate rather than by a comparison
//! written here. A session **token** is stored as a SHA-256 digest of itself:
//! the bearer holds the only copy of the real token, so a store dump, a backup
//! or an over-broad read hands an attacker digests they cannot present.
//!
//! Hashing the token needs no salt and no work factor, and that is not a
//! shortcut: a token is 256 bits of uniform randomness, so there is no dictionary
//! to attack and no guess to slow down. A password is none of those things, which
//! is why it gets Argon2id instead.

use serde::{Deserialize, Serialize};

use crate::{
    meta::{Condition, Timestamp},
    resources::Resource,
};

/// A person (or a service account) that can be granted a role in a project.
///
/// Global, like projects and for the same reason: a user has to mean the same
/// thing in every cell they hold a binding in, and a binding names a subject.
/// Two cells disagreeing about who `alice` is would be two different people with
/// one name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserSpec {
    /// What a person is called on screen. Never used for authorisation — the
    /// subject is the resource id, and a display name that decided anything
    /// would be a second, editable identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,

    /// An address to reach them at. Informational; the id is the identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,

    /// Whether this identity is a **program** rather than a person.
    ///
    /// A cell serving customers needs both, and they differ in exactly one way
    /// that matters: how they prove who they are. A person has a password and
    /// signs in; a program has a token, minted by an operator, shown once and
    /// stored only as a hash — the same shape a node's credential already has,
    /// for the same reason.
    ///
    /// Everything else is deliberately identical. A service account is named in
    /// a project's bindings like anybody else, gets the same four roles, and
    /// appears in the audit trail under its own subject — so "what may this CI
    /// system do here" is answered by reading the project, not by looking for a
    /// token in a file somewhere and guessing.
    ///
    /// Before this, a service account *was* a line in a static token file: no
    /// object, no bindings, no audit, and no way to take one away except by
    /// editing a file and restarting the API.
    #[serde(default)]
    pub service: bool,

    /// A disabled account keeps its bindings and cannot sign in, and its live
    /// sessions stop being accepted on the next request.
    ///
    /// Disabling rather than deleting is the operation an operator actually
    /// wants: deleting a user strands every binding that names them, and a
    /// binding naming a subject that no longer exists is a grant nobody can
    /// audit.
    #[serde(default)]
    pub disabled: bool,

    /// Whether this user may act anywhere in the cell, including inside every
    /// project.
    ///
    /// Deliberately a field on the user rather than only the started-with
    /// operator list: an installation has to be able to appoint an
    /// administrator without a restart. The started-with list stays as well and
    /// stays superior — it is the escape hatch for a cell whose stored admins
    /// are all disabled, and a cell nobody can repair is the failure this
    /// prevents.
    #[serde(default)]
    pub cell_admin: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserStatus {
    pub observed_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// When this account last authenticated. Written by the login path, which is
    /// why it is status and not spec.
    #[serde(default)]
    pub last_login: Timestamp,
}

pub type User = Resource<UserSpec, UserStatus>;

/// One user's password, in a collection the API does not serve.
///
/// Same id as the user it belongs to, so there is nothing to join and no way for
/// the two to drift apart. A credential whose user is gone is dead weight rather
/// than a way in — every path that verifies one loads the user first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CredentialSpec {
    /// An Argon2id PHC string: algorithm, parameters and salt travel with the
    /// digest, so raising the work factor later does not invalidate what is
    /// already stored — an old hash keeps verifying with the parameters it was
    /// made with.
    pub password_hash: String,
    /// When it was last changed, for an operator answering "how old is this".
    #[serde(default)]
    pub updated_at: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CredentialStatus {
    pub observed_generation: u64,
}

pub type Credential = Resource<CredentialSpec, CredentialStatus>;

/// A signed-in session, keyed by the **digest** of its bearer token.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    /// The user this session speaks for.
    pub subject: String,
    /// After this, the session is refused. Checked on every request rather than
    /// swept by a job: a sweeper that falls behind extends every session it has
    /// not reached yet, which is the opposite of what an expiry is for.
    pub expires_at: Timestamp,
    /// When it was issued, so an operator can see a session's whole life.
    #[serde(default)]
    pub issued_at: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub observed_generation: u64,
}

pub type Session = Resource<SessionSpec, SessionStatus>;

/// A per-node agent credential, keyed by the **digest** of its bearer token.
///
/// The node-identity analogue of a [`Session`], and separated from the node for
/// the same reasons a password is separated from the user: it lives in a
/// collection the API does not serve, so a node token cannot leak through a
/// route that does not exist, and only its SHA-256 digest is stored, so a store
/// dump hands an attacker digests they cannot present. The record names the node
/// the token speaks for; verification is a single keyed read on the digest.
///
/// **A node cannot rotate its own credential.** This is a `spec`, which only a
/// controller writes ([`crate::access`]), in a collection with no route — so
/// nothing a node can reach touches it. That is the whole point of putting the
/// digest here rather than on the node object: a node writes its own status, and
/// a credential that lived in that status would be a credential the node could
/// rewrite. Rotation is a cell operator minting a new token, which mints a new
/// record; the old digest stops being accepted when its record is deleted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeCredentialSpec {
    /// The node this token authenticates as. Reading the credential yields the
    /// identity `Writer::agent(node)` is built from.
    pub node: String,
    /// When it was issued, so an operator can see a credential's age.
    #[serde(default)]
    pub issued_at: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeCredentialStatus {
    pub observed_generation: u64,
}

pub type NodeCredential = Resource<NodeCredentialSpec, NodeCredentialStatus>;

/// A token a service account authenticates with.
///
/// The same shape as a node's, and stored the same way: keyed by the token's
/// digest, in a collection with no route, written as a controller. The holder
/// cannot read it back, cannot rotate it, and cannot enumerate the others —
/// rotation is an operator minting a new one, and revocation is deleting a
/// record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceCredentialSpec {
    /// The account this token speaks for.
    pub user: String,
    /// What it is for, in the words of whoever minted it — `deploy pipeline`,
    /// `nightly backups`. An operator looking at four tokens for one account
    /// needs to know which is which before revoking one.
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub issued_at: Timestamp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ServiceCredentialStatus {
    pub observed_generation: u64,
}

pub type ServiceCredential = Resource<ServiceCredentialSpec, ServiceCredentialStatus>;

impl crate::resources::Assigned for UserSpec {}
impl crate::resources::Assigned for CredentialSpec {}
impl crate::resources::Assigned for SessionSpec {}
impl crate::resources::Assigned for NodeCredentialSpec {}
impl crate::resources::Assigned for ServiceCredentialSpec {}

impl crate::resources::Observed for ServiceCredentialStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &[]
    }
    /// Nobody: a credential is a record of an issue, and no agent reports on
    /// one.
    fn owner(&self) -> Option<&str> {
        None
    }
    fn written_by_the_platform(&self) -> bool {
        true
    }
}

impl crate::resources::Observed for NodeCredentialStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &[]
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

impl crate::resources::Observed for UserStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &self.conditions
    }
    /// Nobody. A user is not reported on by an agent — the login path writes
    /// `last_login` and that is the whole of its status.
    fn owner(&self) -> Option<&str> {
        None
    }
    /// And because nobody does, the platform may: without this the login path
    /// was refused on every sign-in and `lastLogin` was a field nothing in the
    /// system could write.
    fn written_by_the_platform(&self) -> bool {
        true
    }
}

impl crate::resources::Observed for CredentialStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &[]
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

impl crate::resources::Observed for SessionStatus {
    fn observed_generation(&self) -> u64 {
        self.observed_generation
    }
    fn conditions(&self) -> &[Condition] {
        &[]
    }
    fn owner(&self) -> Option<&str> {
        None
    }
}

/// How long a fresh session lasts.
///
/// Eight hours: long enough to be a working day, short enough that a token
/// copied off a shared machine stops working before the next one.
pub const SESSION_LIFETIME_MS: u64 = 8 * 60 * 60 * 1000;

/// The error a caller is allowed to see when authentication fails.
///
/// One variant, on purpose. "No such user", "wrong password" and "disabled" are
/// three different facts and telling them apart tells an attacker which
/// usernames exist. The *log* may say which; the caller may not.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("those credentials were not accepted")]
pub struct Rejected;

/// Hash a password for storage.
pub fn hash_password(password: &str) -> Result<String, Rejected> {
    use argon2::{
        Argon2,
        password_hash::{PasswordHasher, SaltString, rand_core::OsRng},
    };
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| Rejected)
}

/// Whether `password` is the one behind `stored`.
///
/// Constant-time inside the `argon2` crate. A malformed stored hash is a
/// rejection rather than an error, because the two are the same thing to a
/// caller and distinguishing them here would put the shape of the stored value
/// into an error message.
pub fn verify_password(password: &str, stored: &str) -> bool {
    use argon2::{
        Argon2,
        password_hash::{PasswordHash, PasswordVerifier},
    };
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// The rule a new password has to meet.
///
/// Length only, and deliberately: composition rules ("one digit, one symbol")
/// are known to push people toward `Password1!` and are not in NIST SP 800-63B
/// any more. Length is the one requirement that buys entropy without buying a
/// predictable shape.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Whether a proposed password may be stored, with the reason if not.
///
/// The reason is safe to show: it describes the *rule*, which is public, not the
/// secret.
pub fn check_password_strength(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!(
            "a password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    Ok(())
}

/// A fresh 256-bit bearer token from the OS, rendered as sixty-four hex
/// characters.
///
/// Returned once and never stored — [`token_digest`] is what goes to the store.
/// Hex rather than base64: it survives every URL, header, shell and log without
/// an encoding question, and sixty-four characters is not a burden for something
/// no person types.
///
/// The same generator serves a session token and a per-node agent token, because
/// a token is a token — 256 bits of uniform randomness, stored only as its
/// digest. The two differ in what record the digest keys, not in the secret.
pub fn new_bearer_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh session token. See [`new_bearer_token`].
pub fn new_session_token() -> String {
    new_bearer_token()
}

/// A fresh per-node agent token, shown once at registration and stored only as
/// its digest in a [`NodeCredential`]. See [`new_bearer_token`].
pub fn new_node_token() -> String {
    new_bearer_token()
}

/// The store key for a bearer token: its SHA-256 digest, hex.
pub fn token_digest(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a session is still valid at `now`, given the user it speaks for.
///
/// Takes the user rather than only the session, because a disabled account has
/// to stop working *now* and not when its sessions happen to expire. That is the
/// difference between disabling an account and asking somebody to please sign
/// out.
pub fn session_is_live(session: &SessionSpec, user: Option<&UserSpec>, now: Timestamp) -> bool {
    let Some(user) = user else {
        // The account was deleted out from under the session.
        return false;
    };
    !user.disabled && session.expires_at.0 > now.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let stored = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &stored));
        assert!(!verify_password("Correct horse battery staple", &stored));
        assert!(!verify_password("", &stored));
    }

    #[test]
    fn two_hashes_of_one_password_differ() {
        // Salted, so a store dump does not reveal that two accounts share a
        // password — and a precomputed table is useless against either.
        let a = hash_password("correct horse battery staple").unwrap();
        let b = hash_password("correct horse battery staple").unwrap();
        assert_ne!(a, b);
        assert!(verify_password("correct horse battery staple", &a));
        assert!(verify_password("correct horse battery staple", &b));
    }

    #[test]
    fn a_malformed_stored_hash_is_a_rejection_not_a_panic() {
        // What a hand-edited store, a truncated write or a botched migration
        // looks like. It must refuse, not crash and not accept.
        for junk in ["", "not-a-phc-string", "$argon2id$v=19$", "$2y$10$abc"] {
            assert!(!verify_password("anything", junk), "{junk:?}");
        }
    }

    #[test]
    fn a_token_is_not_stored_in_the_form_it_is_presented() {
        let token = new_session_token();
        assert_eq!(token.len(), 64);
        let digest = token_digest(&token);
        assert_ne!(digest, token);
        // Deterministic, so a presented token finds its session...
        assert_eq!(digest, token_digest(&token));
        // ...and distinct, so one session's digest is not another's key.
        assert_ne!(digest, token_digest(&new_session_token()));
    }

    #[test]
    fn two_tokens_are_never_the_same() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(seen.insert(new_session_token()), "a token repeated");
        }
    }

    #[test]
    fn a_disabled_account_kills_its_live_sessions_immediately() {
        let now = Timestamp(1_000);
        let session = SessionSpec {
            subject: "alice".into(),
            expires_at: Timestamp(now.0 + SESSION_LIFETIME_MS),
            issued_at: now,
        };
        let live = UserSpec::default();
        assert!(session_is_live(&session, Some(&live), now));

        // Not "at the next expiry" — now. Otherwise disabling an account is a
        // request rather than an act, and the window is the session lifetime.
        let disabled = UserSpec {
            service: false,
            disabled: true,
            ..UserSpec::default()
        };
        assert!(!session_is_live(&session, Some(&disabled), now));

        // A deleted account, likewise.
        assert!(!session_is_live(&session, None, now));

        // And an expired session, with a live account.
        assert!(!session_is_live(
            &session,
            Some(&live),
            Timestamp(session.expires_at.0)
        ));
    }

    #[test]
    fn the_length_rule_is_the_only_rule() {
        assert!(check_password_strength("correct horse battery staple").is_ok());
        // Exactly at the boundary passes; one short does not.
        assert!(check_password_strength(&"a".repeat(MIN_PASSWORD_LEN)).is_ok());
        let err = check_password_strength(&"a".repeat(MIN_PASSWORD_LEN - 1)).unwrap_err();
        assert!(err.contains("at least"), "{err}");
        // No composition rule: a long passphrase of one character class is fine,
        // which is the whole point of dropping them.
        assert!(check_password_strength("aaaaaaaaaaaaaaaaaaaa").is_ok());
        // Counted in characters, not bytes, so a short multi-byte password is
        // not accepted because UTF-8 made it look long.
        assert!(check_password_strength("äöüäöüäöü").is_err());
    }
}
