//! Signing in, and the four ways it must refuse.
//!
//! Authentication is the one surface where a test that only checks the happy
//! path is worse than none: every interesting property here is a *refusal*, and
//! a refusal that quietly stopped working looks exactly like a system that
//! works.

use std::sync::Arc;

use velstra_cloud_api::{
    StaticTokenVerifier, TokenVerifier,
    sessions::{IdentityStore, StoreTokenVerifier, is_cell_admin},
};
use velstra_cloud_model::{
    identity::{User, UserSpec, UserStatus},
    meta::{Meta, Placement, ResourceName, Timestamp},
};
use velstra_cloud_store::{MemoryStore, Store, TypedStore};

const PASSWORD: &str = "correct horse battery staple";

fn identity_store() -> (Arc<dyn Store>, IdentityStore) {
    let store: Arc<dyn Store> = Arc::new(MemoryStore::new());
    let identity = IdentityStore::new(store.clone(), "r1", "cell-1");
    (store, identity)
}

async fn make_user(store: &Arc<dyn Store>, name: &str, spec: UserSpec) {
    let users: TypedStore<UserSpec, UserStatus> = TypedStore::new(store.clone(), "cell-1", "users");
    let user = User {
        meta: Meta::new(
            ResourceName::parse(&format!("users/{name}")).unwrap(),
            Placement::new("r1", "cell-1"),
        ),
        spec,
        status: UserStatus::default(),
    };
    users
        .create(
            &user,
            &velstra_cloud_model::access::Writer::controller("test"),
        )
        .await
        .unwrap();
}

async fn user_with_password(store: &Arc<dyn Store>, identity: &IdentityStore, name: &str) {
    make_user(store, name, UserSpec::default()).await;
    identity.set_password(name, PASSWORD).await.unwrap();
}

#[tokio::test]
async fn a_correct_password_yields_a_token_that_identifies_its_owner() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;

    let signed_in = identity.sign_in("ada", PASSWORD).await.unwrap();
    assert_eq!(signed_in.subject, "ada");
    assert!(!signed_in.cell_admin);
    // Sixty-four hex characters of OS randomness, handed over once.
    assert_eq!(signed_in.token.len(), 64);

    let verifier = StoreTokenVerifier::new(identity.clone());
    let who = verifier.verify(&signed_in.token).await.unwrap();
    assert_eq!(who.subject, "ada");
    assert!(!is_cell_admin(&who));
}

/// The four refusals, and they all say the same thing on purpose.
///
/// A message that distinguished "no such user" from "wrong password" would tell
/// whoever is asking which usernames exist, which is the first half of an attack
/// on the second half.
#[tokio::test]
async fn every_failed_sign_in_refuses_in_the_same_words() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    make_user(&store, "no-password", UserSpec::default()).await;
    make_user(
        &store,
        "disabled",
        UserSpec {
            disabled: true,
            ..UserSpec::default()
        },
    )
    .await;
    identity.set_password("disabled", PASSWORD).await.unwrap();

    let mut messages = Vec::new();
    for (user, password) in [
        ("ada", "wrong password entirely"),
        ("nobody", PASSWORD),
        ("no-password", PASSWORD),
        ("disabled", PASSWORD),
    ] {
        let err = identity.sign_in(user, password).await.unwrap_err();
        assert_eq!(err.code, velstra_cloud_api::Code::Unauthenticated, "{user}");
        messages.push(err.message);
    }
    let first = &messages[0];
    for message in &messages {
        assert_eq!(message, first, "the refusals differ: {messages:?}");
    }
}

/// Disabling an account ends its live sessions **now**, not at their expiry.
///
/// The alternative — letting existing tokens run out — makes disabling an
/// account a request rather than an act, and the window is the session lifetime.
/// Which is to say: the person you just disabled keeps working for eight hours.
#[tokio::test]
async fn disabling_an_account_stops_a_token_it_already_issued() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let token = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let verifier = StoreTokenVerifier::new(identity.clone());
    assert!(verifier.verify(&token).await.is_ok());

    let users: TypedStore<UserSpec, UserStatus> = TypedStore::new(store.clone(), "cell-1", "users");
    let mut ada = users.get("users/ada").await.unwrap().unwrap();
    ada.spec.disabled = true;
    ada.meta.generation += 1;
    users
        .update(&ada, &velstra_cloud_model::Writer::controller("test"))
        .await
        .unwrap();

    assert_eq!(
        verifier.verify(&token).await.unwrap_err().code,
        velstra_cloud_api::Code::Unauthenticated,
        "a disabled account kept working"
    );
}

/// Changing a password ends every session issued under the old one.
///
/// This is the whole reason a person changes a password after it leaks, so a
/// change that left the leaked session alive would answer the wrong problem.
#[tokio::test]
async fn changing_a_password_ends_the_sessions_it_replaces() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let old = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let second = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let verifier = StoreTokenVerifier::new(identity.clone());
    assert!(verifier.verify(&old).await.is_ok());

    identity
        .set_password("ada", "an entirely different passphrase")
        .await
        .unwrap();

    // Both, not just the one that happened to be looked at.
    for token in [&old, &second] {
        assert_eq!(
            verifier.verify(token).await.unwrap_err().code,
            velstra_cloud_api::Code::Unauthenticated,
        );
    }
    // ...and the new password works.
    assert!(
        identity
            .sign_in("ada", "an entirely different passphrase")
            .await
            .is_ok()
    );
    assert!(identity.sign_in("ada", PASSWORD).await.is_err());
}

#[tokio::test]
async fn signing_out_revokes_that_session_and_only_that_one() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let laptop = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let phone = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let verifier = StoreTokenVerifier::new(identity.clone());

    identity.sign_out(&laptop).await.unwrap();
    assert!(verifier.verify(&laptop).await.is_err());
    assert!(
        verifier.verify(&phone).await.is_ok(),
        "signing out of one place signed out of the other"
    );

    // Idempotent: the caller wanted it gone and it is.
    identity.sign_out(&laptop).await.unwrap();
    identity
        .sign_out("a token nobody ever issued")
        .await
        .unwrap();
}

/// A cell operator is a fact about the user record, resolved once at
/// authentication and carried on the identity.
#[tokio::test]
async fn an_administrator_is_marked_on_the_identity_they_authenticate_as() {
    let (store, identity) = identity_store();
    make_user(
        &store,
        "root",
        UserSpec {
            cell_admin: true,
            ..UserSpec::default()
        },
    )
    .await;
    identity.set_password("root", PASSWORD).await.unwrap();

    let signed_in = identity.sign_in("root", PASSWORD).await.unwrap();
    assert!(signed_in.cell_admin);
    let verifier = StoreTokenVerifier::new(identity.clone());
    assert!(is_cell_admin(
        &verifier.verify(&signed_in.token).await.unwrap()
    ));
}

/// A password is never stored in a form that can be presented, and neither is a
/// token.
///
/// Asserted against the raw store rather than through an accessor: the property
/// is about what a backup, a dump or an over-broad read contains, and an
/// accessor could redact where the bytes do not.
#[tokio::test]
async fn neither_secret_is_stored_in_the_form_it_is_presented() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let token = identity.sign_in("ada", PASSWORD).await.unwrap().token;

    let everything = store.list("").await.unwrap();
    let dump = everything
        .iter()
        .map(|kv| String::from_utf8_lossy(&kv.value).to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !dump.contains(PASSWORD),
        "the password is in the store in the clear"
    );
    assert!(
        !dump.contains(&token),
        "the session token is in the store in the clear"
    );
    // What *is* there is an Argon2id hash — so the parameters travel with it and
    // raising them later does not invalidate what is stored.
    assert!(dump.contains("$argon2id$"), "not an argon2id hash: {dump}");
}

/// A short password is refused, and refused with the rule rather than a shrug.
#[tokio::test]
async fn a_password_that_cannot_be_stored_says_why() {
    let (store, identity) = identity_store();
    make_user(&store, "ada", UserSpec::default()).await;
    let err = identity.set_password("ada", "short").await.unwrap_err();
    assert_eq!(err.code, velstra_cloud_api::Code::InvalidArgument);
    assert!(err.message.contains("at least"), "{}", err.message);
    // And nothing was stored, so the account is still without a password rather
    // than with a weak one.
    assert!(identity.sign_in("ada", "short").await.is_err());
}

/// Bootstrapping creates the first administrator and never a second one.
///
/// The guard is "no users at all", not "this user is missing". Re-running a
/// bootstrap against a populated cell must not resurrect a deleted
/// administrator or reset a live one's password — that would be an
/// unauthenticated way back in for anyone who can restart the process.
#[tokio::test]
async fn bootstrapping_happens_once_and_never_again() {
    let (store, identity) = identity_store();

    assert!(identity.bootstrap_admin("root", PASSWORD).await.unwrap());
    let signed_in = identity.sign_in("root", PASSWORD).await.unwrap();
    assert!(signed_in.cell_admin);

    // A second run does nothing, even with a different password.
    assert!(
        !identity
            .bootstrap_admin("root", "a completely different one")
            .await
            .unwrap()
    );
    assert!(
        identity.sign_in("root", PASSWORD).await.is_ok(),
        "bootstrap reset a live administrator's password"
    );
    assert!(
        identity
            .sign_in("root", "a completely different one")
            .await
            .is_err()
    );

    // And not even once the administrator is deleted, as long as anyone is left:
    // a cell with users is a cell somebody already owns.
    make_user(&store, "ada", UserSpec::default()).await;
    assert!(
        !identity
            .bootstrap_admin("intruder", "another passphrase here")
            .await
            .unwrap()
    );
    assert!(identity.user("intruder").await.unwrap().is_none());
}

/// A static token and a session token both work, and are told apart by trying
/// the session first.
///
/// This is what lets an agent keep a long-lived token while people sign in with
/// passwords — two different kinds of caller, which one mechanism would serve
/// badly in one direction or the other.
#[tokio::test]
async fn a_service_token_and_a_person_can_share_one_api() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let session = identity.sign_in("ada", PASSWORD).await.unwrap().token;

    let fallback: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::new([(
        "agent-token".to_string(),
        velstra_cloud_api::Identity::new("node-a"),
    )]));
    let verifier = StoreTokenVerifier::new(identity.clone()).with_fallback(fallback);

    assert_eq!(verifier.verify(&session).await.unwrap().subject, "ada");
    assert_eq!(
        verifier.verify("agent-token").await.unwrap().subject,
        "node-a"
    );
    assert_eq!(
        verifier.verify("neither").await.unwrap_err().code,
        velstra_cloud_api::Code::Unauthenticated
    );
}

/// Changing your own password keeps you signed in where you are, and signs you
/// out everywhere else.
///
/// Both halves matter and they pull in opposite directions. Ending every
/// session is what makes a change after a leak mean something; keeping the
/// current one is what stops a routine change from throwing an operator out of
/// the console they are working in.
#[tokio::test]
async fn changing_your_own_password_keeps_the_session_you_are_in() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let here = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let elsewhere = identity.sign_in("ada", PASSWORD).await.unwrap().token;
    let verifier = StoreTokenVerifier::new(identity.clone());

    identity
        .set_password_keeping("ada", "a different passphrase entirely", Some(&here))
        .await
        .unwrap();

    assert!(
        verifier.verify(&here).await.is_ok(),
        "changing your own password signed you out of the console you did it from"
    );
    assert!(
        verifier.verify(&elsewhere).await.is_err(),
        "a session issued under the old password survived"
    );

    // An operator resetting somebody else's ends every one of theirs, which is
    // the point when a door is being shut.
    let ada_again = identity
        .sign_in("ada", "a different passphrase entirely")
        .await
        .unwrap()
        .token;
    identity
        .set_password("ada", "reset by an operator now")
        .await
        .unwrap();
    assert!(verifier.verify(&ada_again).await.is_err());
    assert!(verifier.verify(&here).await.is_err());
}

/// The periodic sweeper deletes a session once it has expired, and not before.
///
/// The request path only sweeps a token that is presented again; a token issued
/// and never used once more is exactly what this reaper exists to reach, so the
/// property worth pinning is that it deletes an expired record and leaves a live
/// one alone. The clock is injected — the moment a session stops being accepted
/// (`now >= expires_at`) is the moment it becomes sweepable, and the two must
/// agree or a live session is reaped or a dead one lingers.
#[tokio::test]
async fn the_sweeper_reaps_expired_sessions_and_keeps_live_ones() {
    let (store, identity) = identity_store();
    user_with_password(&store, &identity, "ada").await;
    let signed_in = identity.sign_in("ada", PASSWORD).await.unwrap();
    let expires_at = signed_in.expires_at;

    // A second, still-live session that must survive every sweep below — so the
    // reaper is shown deleting one record and not simply emptying the store.
    let live = identity.sign_in("ada", PASSWORD).await.unwrap();

    // One tick before expiry: nothing is swept, and the token still stands.
    let swept = identity
        .sweep_expired_sessions(Timestamp(expires_at - 1))
        .await
        .unwrap();
    assert_eq!(swept, 0, "a session was reaped while it was still live");
    assert!(identity.session_present(&signed_in.token).await);

    // At expiry — the same instant the request path stops accepting it — the
    // record is deleted and the token is gone from the store.
    let swept = identity
        .sweep_expired_sessions(Timestamp(expires_at))
        .await
        .unwrap();
    assert_eq!(swept, 1, "the expired session was not reaped");
    assert!(!identity.session_present(&signed_in.token).await);
    // The live session, issued a moment later, is untouched.
    assert!(
        identity.session_present(&live.token).await,
        "the reaper took a live session with the expired one"
    );

    // Idempotent: with the expired record gone, a second sweep at the same clock
    // finds nothing to do.
    assert_eq!(
        identity
            .sweep_expired_sessions(Timestamp(expires_at))
            .await
            .unwrap(),
        0
    );
}
