//! Signing in, signing out, and the identity a request carries afterwards.
//!
//! ## Why this is not another collection
//!
//! [`Credential`] and [`Session`] are stored objects that the API deliberately
//! never serves. They are not in `COLLECTIONS`, so there is no `GET`, no `LIST`,
//! no `WATCH` and no proxy hop that can reach them — a password hash and a live
//! session cannot leak through a route that does not exist. Everything here
//! reaches them through a typed store directly, and every function that does is
//! in this file, which is a short enough list to audit.
//!
//! ## What a request costs
//!
//! One store read per request, for the session, plus one for the user — and the
//! user read is what makes disabling an account take effect immediately rather
//! than at the next expiry. That is two reads on a path that already does at
//! least one, and it is the price of revocation being a fact rather than a
//! promise. A cell that outgrows it caches the pair with a short TTL, and the
//! TTL is then exactly the window in which a disabled account still works —
//! which is a decision to make deliberately, not to arrive at by accident.
//!
//! ## What is deliberately not here
//!
//! **No refresh tokens.** A session that can mint another session is a
//! credential with a longer life than the one it came from, and the reason to
//! want one — not making people sign in again — is better served by a longer
//! session that can be revoked.
//!
//! **No lockout after N failures.** It is the obvious next thing and it is a
//! denial-of-service primitive: anyone who knows a username can lock its owner
//! out. Rate limiting the *caller* is the right shape and belongs in front of
//! the API, where it can see an address.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use velstra_cloud_model::{
    identity::{
        Credential, CredentialSpec, CredentialStatus, SESSION_LIFETIME_MS, SessionSpec,
        SessionStatus, User, UserSpec, UserStatus, check_password_strength, hash_password,
        new_session_token, session_is_live, token_digest, verify_password,
    },
    meta::{Meta, Placement, Timestamp},
    resources::Resource,
};
use velstra_cloud_store::{Store, StoreError, TypedStore, typed::TypedError};

use crate::{
    auth::{Identity, TokenVerifier},
    error::{ApiError, ApiResult, Code},
};

/// The claim that says a subject may act anywhere in the cell.
///
/// Resolved once, at authentication, and carried on the [`Identity`] — rather
/// than read from the user record on every authorisation check. Authorisation
/// runs at the top of every entry point; a store read there would put one on the
/// hot path of every request, to answer a question whose answer cannot change
/// between the start and the end of the same request.
pub const CELL_ADMIN_SCOPE: &str = "cell-admin";

/// Whether an authenticated caller holds cell-wide authority.
pub fn is_cell_admin(who: &Identity) -> bool {
    who.scopes.iter().any(|s| s == CELL_ADMIN_SCOPE)
}

/// What a successful sign-in hands back.
///
/// camelCase on the wire, like every other body this API serves. A field that
/// arrived as `cell_admin` where the contract says `cellAdmin` reads as absent
/// to a console, and "absent" for this one means "not an operator" — so the
/// spelling decides what somebody is shown.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignedIn {
    /// The bearer token. Returned **once**; only its digest is stored, so it
    /// cannot be recovered from the cell afterwards.
    pub token: String,
    pub subject: String,
    pub display_name: String,
    pub cell_admin: bool,
    pub expires_at: u64,
}

/// The store handles for the two collections the API does not serve.
///
/// Cloned into the verifier and into the API, so both reach exactly the same
/// objects and neither can hold a stale idea of the other's.
#[derive(Clone)]
pub struct IdentityStore {
    users: TypedStore<UserSpec, UserStatus>,
    credentials: TypedStore<CredentialSpec, CredentialStatus>,
    sessions: TypedStore<SessionSpec, SessionStatus>,
    placement: Placement,
}

impl IdentityStore {
    pub fn new(store: Arc<dyn Store>, region: &str, cell: &str) -> Self {
        Self {
            users: TypedStore::new(store.clone(), cell, "users"),
            credentials: TypedStore::new(store.clone(), cell, "credentials"),
            sessions: TypedStore::new(store, cell, "sessions"),
            placement: Placement::new(region, cell),
        }
    }

    fn now() -> Timestamp {
        Timestamp::now()
    }

    /// Exchange a username and password for a session token.
    ///
    /// Every failure returns the same message. "No such user", "wrong password"
    /// and "this account is disabled" are three different facts, and telling
    /// them apart tells whoever is asking which usernames exist — so the caller
    /// gets one sentence and the operator gets the detail in the log.
    pub async fn sign_in(&self, username: &str, password: &str) -> ApiResult<SignedIn> {
        let rejected = || {
            ApiError::new(
                Code::Unauthenticated,
                "that username and password were not accepted",
            )
        };

        let user = self
            .users
            .get(&stored("users", username))
            .await
            .map_err(store_error)?;
        let Some(user) = user else {
            // Hash anyway. Returning early on an unknown username makes the
            // response measurably faster than for a known one, and that
            // difference is a username oracle — the one thing the identical
            // message above exists to prevent.
            let _ = verify_password(password, DUMMY_HASH);
            tracing::info!(user = username, "sign-in refused: no such user");
            return Err(rejected());
        };
        if user.spec.disabled {
            let _ = verify_password(password, DUMMY_HASH);
            tracing::info!(user = username, "sign-in refused: account disabled");
            return Err(rejected());
        }
        let credential = self
            .credentials
            .get(&stored("credentials", username))
            .await
            .map_err(store_error)?;
        let Some(credential) = credential else {
            let _ = verify_password(password, DUMMY_HASH);
            tracing::warn!(user = username, "sign-in refused: account has no password");
            return Err(rejected());
        };
        if !verify_password(password, &credential.spec.password_hash) {
            tracing::info!(user = username, "sign-in refused: wrong password");
            return Err(rejected());
        }

        let now = Self::now();
        let token = new_session_token();
        let session = Resource {
            meta: Meta::new(
                name_of("sessions", &token_digest(&token))?,
                self.placement.clone(),
            ),
            spec: SessionSpec {
                subject: username.to_string(),
                expires_at: Timestamp(now.0 + SESSION_LIFETIME_MS),
                issued_at: now,
            },
            status: SessionStatus::default(),
        };
        self.sessions.create(&session).await.map_err(store_error)?;

        // Best-effort: a sign-in that worked must not fail because the record of
        // it could not be written.
        let mut touched = user.clone();
        touched.status.last_login = now;
        if let Err(e) = self
            .users
            .update(
                &touched,
                &velstra_cloud_model::Writer::controller("sign-in"),
            )
            .await
        {
            tracing::warn!(user = username, "could not record the sign-in: {e}");
        }

        tracing::info!(user = username, "signed in");
        Ok(SignedIn {
            token,
            subject: username.to_string(),
            display_name: user.spec.display_name.clone(),
            cell_admin: user.spec.cell_admin,
            expires_at: session.spec.expires_at.0,
        })
    }

    /// Revoke one session. Idempotent: signing out twice is signing out once,
    /// and a token that was already gone is not an error to the person holding
    /// it — they wanted it gone and it is.
    pub async fn sign_out(&self, token: &str) -> ApiResult<()> {
        let digest = token_digest(token);
        let existing = self
            .sessions
            .get(&stored("sessions", &digest))
            .await
            .map_err(store_error)?;
        if let Some(session) = existing {
            self.sessions
                .delete(&stored("sessions", &digest), session.meta.revision)
                .await
                .map_err(store_error)?;
            tracing::info!(user = session.spec.subject, "signed out");
        }
        Ok(())
    }

    /// Revoke every session a subject holds.
    pub async fn revoke_all(&self, subject: &str) -> ApiResult<usize> {
        self.revoke_all_except(subject, None).await
    }

    /// Revoke every session a subject holds **except** the one presented.
    ///
    /// What a password change needs: leaving the old sessions alive would mean
    /// the change did not take effect for whoever already had a token, which is
    /// precisely the person it was aimed at. Keeping the *current* one is not a
    /// hole in that — it belongs to whoever just proved they know the new
    /// password — and ending it would sign an operator out of the console at the
    /// moment they finished a routine task.
    ///
    /// `keep` is a raw token, not a digest, because that is what the caller has.
    pub async fn revoke_all_except(&self, subject: &str, keep: Option<&str>) -> ApiResult<usize> {
        let spare = keep.map(token_digest);
        let sessions = self.sessions.list().await.map_err(store_error)?;
        let mut revoked = 0;
        for session in sessions
            .iter()
            .filter(|s| s.spec.subject == subject)
            .filter(|s| Some(s.meta.name.id()) != spare.as_deref())
        {
            if self
                .sessions
                .delete(&session.meta.name.to_string(), session.meta.revision)
                .await
                .is_ok()
            {
                revoked += 1;
            }
        }
        if revoked > 0 {
            tracing::info!(user = subject, revoked, "revoked sessions");
        }
        Ok(revoked)
    }

    /// Set (or replace) a user's password, ending every session it replaces.
    pub async fn set_password(&self, username: &str, password: &str) -> ApiResult<()> {
        self.set_password_keeping(username, password, None).await
    }

    /// The same, keeping the session `keep` belongs to.
    ///
    /// Used when somebody changes their own password from the console: every
    /// other session ends, and the one they are sitting in does not.
    pub async fn set_password_keeping(
        &self,
        username: &str,
        password: &str,
        keep: Option<&str>,
    ) -> ApiResult<()> {
        check_password_strength(password).map_err(ApiError::invalid)?;
        if self
            .users
            .get(&stored("users", username))
            .await
            .map_err(store_error)?
            .is_none()
        {
            return Err(ApiError::not_found(format!("users/{username}")));
        }
        let hash = hash_password(password)
            .map_err(|_| ApiError::new(Code::Internal, "the password could not be hashed"))?;
        let now = Self::now();
        let existing = self
            .credentials
            .get(&stored("credentials", username))
            .await
            .map_err(store_error)?;
        match existing {
            Some(mut credential) => {
                credential.spec.password_hash = hash;
                credential.spec.updated_at = now;
                credential.meta.generation += 1;
                self.credentials
                    .update(
                        &credential,
                        &velstra_cloud_model::Writer::controller("set-password"),
                    )
                    .await
                    .map_err(store_error)?;
            }
            None => {
                let credential = Credential {
                    meta: Meta::new(name_of("credentials", username)?, self.placement.clone()),
                    spec: CredentialSpec {
                        password_hash: hash,
                        updated_at: now,
                    },
                    status: CredentialStatus::default(),
                };
                self.credentials
                    .create(&credential)
                    .await
                    .map_err(store_error)?;
            }
        }
        // The new password is not in force for anyone still holding a token
        // issued under the old one, unless those tokens stop working.
        self.revoke_all_except(username, keep).await?;
        tracing::info!(user = username, "password changed");
        Ok(())
    }

    /// Remove a user's credential and sessions. Called when the user is deleted.
    pub async fn forget(&self, username: &str) -> ApiResult<()> {
        if let Some(credential) = self
            .credentials
            .get(&stored("credentials", username))
            .await
            .map_err(store_error)?
        {
            let _ = self
                .credentials
                .delete(&stored("credentials", username), credential.meta.revision)
                .await;
        }
        self.revoke_all(username).await?;
        Ok(())
    }

    /// Create the first administrator, if and only if there is no user at all.
    ///
    /// A cell with no users is a cell nobody can sign into, and an installer
    /// that leaves one is an installer that did not finish. The guard is "no
    /// users **at all**", not "this user is missing": re-running a bootstrap
    /// against a populated cell must never resurrect a deleted administrator or
    /// reset a live one's password, which is an unauthenticated way back in for
    /// anyone who can restart the process.
    ///
    /// Returns whether it created anything.
    pub async fn bootstrap_admin(&self, username: &str, password: &str) -> ApiResult<bool> {
        if !self.users.list().await.map_err(store_error)?.is_empty() {
            return Ok(false);
        }
        check_password_strength(password).map_err(ApiError::invalid)?;
        let user = User {
            meta: Meta::new(name_of("users", username)?, self.placement.clone()),
            spec: UserSpec {
                display_name: username.to_string(),
                cell_admin: true,
                ..UserSpec::default()
            },
            status: UserStatus::default(),
        };
        match self.users.create(&user).await {
            Ok(_) => {}
            // Two API replicas can both find the cell empty and both try to
            // create the first administrator; the store lets exactly one win.
            // The loser is not an error — the cell got its administrator, which
            // is all a bootstrap promised — so it steps aside idempotently
            // rather than aborting an otherwise healthy start. The winner owns
            // the password; the loser does not touch it.
            Err(TypedError::Store(StoreError::Exists { .. })) => return Ok(false),
            Err(e) => return Err(store_error(e)),
        }
        self.set_password(username, password).await?;
        tracing::info!(user = username, "created the first administrator");
        Ok(true)
    }

    /// The identity a bearer token stands for, or `Unauthenticated`.
    async fn identify(&self, token: &str) -> ApiResult<Identity> {
        let refused = || ApiError::new(Code::Unauthenticated, "the bearer token was not accepted");

        let digest = token_digest(token);
        let Some(session) = self
            .sessions
            .get(&stored("sessions", &digest))
            .await
            .map_err(store_error)?
        else {
            return Err(refused());
        };
        let user = self
            .users
            .get(&stored("users", &session.spec.subject))
            .await
            .map_err(store_error)?;
        if !session_is_live(&session.spec, user.as_ref().map(|u| &u.spec), Self::now()) {
            // Sweep it on the way past. An expired session that stays in the
            // store is answered correctly every time and still grows without
            // bound, and the cheapest place to notice one is where it is read.
            //
            // Known limit: this only sweeps sessions that are *presented* again.
            // A token issued and never used once more — a tab closed without a
            // sign-out — leaves a record that expires but is never read, so
            // nothing here reaches it. The bound is small (one row per such
            // sign-in until a periodic sweeper exists), and the fix when it is
            // wanted is a timer that lists and deletes expired sessions, not a
            // change to this path.
            let _ = self
                .sessions
                .delete(&stored("sessions", &digest), session.meta.revision)
                .await;
            return Err(refused());
        }
        let user = user.expect("session_is_live refuses a session with no user");
        let mut identity = Identity::new(session.spec.subject);
        if user.spec.cell_admin {
            identity.scopes.push(CELL_ADMIN_SCOPE.to_string());
        }
        Ok(identity)
    }

    /// Read a user, for the API's own `users` handling.
    pub async fn user(&self, username: &str) -> ApiResult<Option<User>> {
        self.users
            .get(&stored("users", username))
            .await
            .map_err(store_error)
    }

    /// Whether the presented token is backed by a live session record.
    ///
    /// A static token or a service-account credential is verified by the
    /// fallback and has no session behind it, so there is nothing for a sign-out
    /// to end. The console asks this to decide whether to offer one — a button
    /// that ends a session there is not is a button that does nothing.
    ///
    /// Liveness is checked the same way the request path checks it, so an
    /// expired or disabled session reads as absent rather than present.
    pub async fn session_present(&self, token: &str) -> bool {
        let digest = token_digest(token);
        let Ok(Some(session)) = self.sessions.get(&stored("sessions", &digest)).await else {
            return false;
        };
        let user = self
            .users
            .get(&stored("users", &session.spec.subject))
            .await
            .ok()
            .flatten();
        session_is_live(&session.spec, user.as_ref().map(|u| &u.spec), Self::now())
    }

    /// Confirm a user's current password.
    ///
    /// For a self-service password change: proving the old password is what
    /// separates the account's owner from someone holding a stolen session, so a
    /// change made from a hijacked token cannot be made permanent. Verifies
    /// against [`DUMMY_HASH`] when there is no credential, so a missing password
    /// takes the same time as a wrong one.
    pub async fn verify_current(&self, username: &str, password: &str) -> ApiResult<bool> {
        let credential = self
            .credentials
            .get(&stored("credentials", username))
            .await
            .map_err(store_error)?;
        Ok(match credential {
            Some(credential) => verify_password(password, &credential.spec.password_hash),
            None => {
                let _ = verify_password(password, DUMMY_HASH);
                false
            }
        })
    }
}

/// An Argon2id hash of a value nobody knows, verified against when a username
/// does not exist so that the refusal takes the same time as a wrong password.
///
/// A constant rather than a hash computed at start-up, because the point is the
/// *work*, and the work is in `verify_password` either way.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$\
                          c29tZXNhbHRzb21lc2FsdA$\
                          3n9Q8Zy9Q1n0Yy1Yq6Yq6Yq6Yq6Yq6Yq6Yq6Yq6Yq6Y";

/// A resource name in the platform's own form — `collection/id` — refusing
/// anything a name cannot hold.
///
/// A username arrives from outside and the store keys on it, so this parse is
/// the boundary check rather than a formatting convenience: a name with a slash
/// in it would otherwise address a different collection.
fn name_of(kind: &str, id: &str) -> ApiResult<velstra_cloud_model::meta::ResourceName> {
    velstra_cloud_model::meta::ResourceName::parse(&format!("{kind}/{id}"))
        .map_err(|e| ApiError::invalid(format!("{id:?} is not a usable name: {e}")))
}

/// The store name of a user, a credential or a session.
fn stored(kind: &str, id: &str) -> String {
    format!("{kind}/{id}")
}

fn store_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::new(Code::Internal, format!("the store refused: {e}"))
}

/// A [`TokenVerifier`] whose source of truth is the cell's own session records.
///
/// The point of the trait is that nothing above it can tell this from an OIDC
/// verifier or a token file. That holds here: this one happens to answer from
/// the store, and the day an installation puts an identity provider in front,
/// the file that changes is this one.
pub struct StoreTokenVerifier {
    identity: IdentityStore,
    /// A verifier consulted first, for tokens that are not sessions at all.
    ///
    /// This is how a service account, an agent or a test keeps working while
    /// people sign in with passwords: the two are different kinds of caller and
    /// forcing them through one mechanism would mean either issuing sessions to
    /// daemons or giving people static tokens.
    fallback: Option<Arc<dyn TokenVerifier>>,
}

impl StoreTokenVerifier {
    pub fn new(identity: IdentityStore) -> Self {
        Self {
            identity,
            fallback: None,
        }
    }

    pub fn with_fallback(mut self, fallback: Arc<dyn TokenVerifier>) -> Self {
        self.fallback = Some(fallback);
        self
    }
}

#[async_trait::async_trait]
impl TokenVerifier for StoreTokenVerifier {
    async fn verify(&self, token: &str) -> ApiResult<Identity> {
        match self.identity.identify(token).await {
            Ok(identity) => Ok(identity),
            Err(e) if e.code == Code::Unauthenticated => match &self.fallback {
                Some(fallback) => fallback.verify(token).await,
                None => Err(e),
            },
            Err(e) => Err(e),
        }
    }
}
