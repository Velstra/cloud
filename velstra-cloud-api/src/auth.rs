//! Who is calling, behind a trait.
//!
//! The API knows one thing about a token: whether some verifier accepts it and
//! what identity it stands for. It deliberately cannot tell an OIDC-issued JWT
//! from a line in a file on disk — the day production moves to a different
//! issuer, nothing above this file changes, and a development cell keeps
//! working without pulling an identity provider into a test.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;

use crate::error::{ApiError, ApiResult, Code};

/// Who a request is made by, once the token has been believed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// What goes into `requested_by` on an operation, and into an audit line.
    pub subject: String,
    /// Free-form claims a future authorisation layer will read. Empty here:
    /// this file answers "who", never "may they".
    pub scopes: Vec<String>,
}

impl Identity {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            scopes: Vec::new(),
        }
    }
}

#[async_trait]
pub trait TokenVerifier: Send + Sync + 'static {
    /// Verify a bearer token. The error is always `UNAUTHENTICATED` — a
    /// verifier that reports *why* a token failed is a verifier that helps
    /// somebody guess one.
    async fn verify(&self, token: &str) -> ApiResult<Identity>;
}

/// Tokens from a file, for development and for tests.
///
/// One token per line, optionally `token subject`, blank lines and `#`
/// comments ignored. This is not a weaker kind of authentication than the
/// production one — it is the same interface with a different source of truth,
/// which is the only way to be sure the production path is not a special case.
pub struct StaticTokenVerifier {
    tokens: BTreeMap<String, Identity>,
}

impl StaticTokenVerifier {
    pub fn new(tokens: impl IntoIterator<Item = (String, Identity)>) -> Self {
        Self {
            tokens: tokens.into_iter().collect(),
        }
    }

    /// One anonymous development token.
    pub fn single(token: impl Into<String>) -> Self {
        Self::new([(token.into(), Identity::new("dev"))])
    }

    pub fn from_file_contents(contents: &str) -> ApiResult<Self> {
        let mut tokens = BTreeMap::new();
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (token, subject) = match line.split_once(char::is_whitespace) {
                Some((t, s)) => (t.trim(), s.trim()),
                None => (line, "dev"),
            };
            tokens.insert(token.to_string(), Identity::new(subject));
        }
        if tokens.is_empty() {
            // Starting with no tokens would serve an API nobody can reach, and
            // would look exactly like a permissions bug from the outside.
            return Err(ApiError::invalid("the token file holds no tokens"));
        }
        Ok(Self { tokens })
    }
}

#[async_trait]
impl TokenVerifier for StaticTokenVerifier {
    async fn verify(&self, token: &str) -> ApiResult<Identity> {
        self.tokens.get(token).cloned().ok_or_else(|| {
            ApiError::new(Code::Unauthenticated, "the bearer token was not accepted")
        })
    }
}

/// Pull the token out of an `Authorization: Bearer …` header and verify it.
pub async fn identify(
    verifier: &Arc<dyn TokenVerifier>,
    header: Option<&str>,
) -> ApiResult<Identity> {
    let header = header.ok_or_else(|| {
        ApiError::new(
            Code::Unauthenticated,
            "this API needs an Authorization: Bearer <token> header",
        )
    })?;
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
        .ok_or_else(|| {
            ApiError::new(
                Code::Unauthenticated,
                "the Authorization header is not a bearer token",
            )
        })?;
    verifier.verify(token.trim()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_token_file_names_the_subject_it_stands_for() {
        let v =
            StaticTokenVerifier::from_file_contents("# dev\nsecret-1 alice\nsecret-2\n").unwrap();
        assert_eq!(v.verify("secret-1").await.unwrap().subject, "alice");
        assert_eq!(v.verify("secret-2").await.unwrap().subject, "dev");
        assert_eq!(
            v.verify("guessed").await.unwrap_err().code,
            Code::Unauthenticated
        );
    }

    #[tokio::test]
    async fn a_missing_or_malformed_header_is_unauthenticated_not_a_crash() {
        let v: Arc<dyn TokenVerifier> = Arc::new(StaticTokenVerifier::single("t"));
        assert_eq!(
            identify(&v, None).await.unwrap_err().code,
            Code::Unauthenticated
        );
        assert_eq!(
            identify(&v, Some("Basic dXNlcjpwdw=="))
                .await
                .unwrap_err()
                .code,
            Code::Unauthenticated
        );
        assert_eq!(identify(&v, Some("Bearer t")).await.unwrap().subject, "dev");
    }
}
