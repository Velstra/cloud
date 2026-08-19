//! Answering a collection a page at a time.
//!
//! An unpaged list is the shape that decides how large a cell can get before the
//! API stops being able to answer at all: `GET .../instances` on a cell of ten
//! thousand builds ten thousand objects, computes every derived field on each,
//! and serialises the lot — to answer a console showing twenty rows. The store
//! has paged internally since the beginning (etcd caps a range response by
//! message size); what was missing was letting a caller say how much they wanted.
//!
//! **The token is opaque and it is checked.** It carries where to resume, which
//! collection it came from, and the revision the first page was read at. A token
//! is meaningless against a different collection or parent, and handing one over
//! anyway would answer confidently with the wrong objects, so the mismatch is
//! refused rather than absorbed.
//!
//! **Why the revision travels with it.** Every page of a walk reports the
//! revision of its *first* page, not its own. That is what keeps the
//! list-then-watch pattern correct across a paged list: the caller pages to the
//! end, then watches from the revision it was given, and the watch replays every
//! change since the walk started — including the ones that landed between two
//! pages. Events carry whole objects, so applying them over a slightly torn list
//! converges on the truth. Report each page's own revision instead and the
//! caller watches from the end of the walk, silently missing everything that
//! happened during it.

use base64::Engine as _;

use crate::error::{ApiError, ApiResult};

/// What a caller gets by default when they ask for a list without saying how
/// much of it they want.
///
/// Large enough that a console or a `list` command is one round trip in
/// practice, small enough that the default is not itself the unbounded read this
/// module exists to remove.
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// The most any single answer will carry, whatever was asked for.
///
/// A ceiling rather than an error, which is what AIP-158 asks for: a caller who
/// asks for a million gets a thousand and a token, and their loop still
/// terminates. Refusing instead would break clients over a number they have no
/// way to know.
pub const MAX_PAGE_SIZE: usize = 1000;

/// Where a walk resumes, and what it is a walk of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageToken {
    /// The collection this token was minted against.
    pub kind: String,
    /// The parent the walk was scoped to.
    pub parent: String,
    /// The last key already delivered. The next page starts strictly after it.
    pub after: String,
    /// The revision the *first* page was read at — see the module note.
    pub revision: u64,
}

impl PageToken {
    /// Encode for the wire.
    ///
    /// URL-safe base64 without padding, because this travels as a query
    /// parameter. The encoding is not a secret and is not pretending to be one:
    /// it is here so the value reads as a token rather than as something a
    /// caller is invited to construct.
    pub fn encode(&self) -> String {
        let plain = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.kind, self.parent, self.revision, self.after
        );
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(plain)
    }

    /// Decode, and refuse anything that is not one of ours.
    ///
    /// Every failure here is the caller's, so every failure is an
    /// invalid-argument error naming what was wrong — never a panic and never a
    /// silent fall back to "start from the beginning", which would quietly
    /// restart a walk the caller believed was half done.
    pub fn decode(raw: &str) -> ApiResult<Self> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| ApiError::invalid("page token is not a token this API issued"))?;
        let plain = String::from_utf8(bytes)
            .map_err(|_| ApiError::invalid("page token is not a token this API issued"))?;
        // The unit separator cannot occur in a resource name, so splitting on it
        // is unambiguous even though the last field is itself a key.
        let mut parts = plain.splitn(4, '\u{1f}');
        let (Some(kind), Some(parent), Some(revision), Some(after)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ApiError::invalid(
                "page token is not a token this API issued",
            ));
        };
        let revision = revision
            .parse::<u64>()
            .map_err(|_| ApiError::invalid("page token is not a token this API issued"))?;
        Ok(Self {
            kind: kind.to_string(),
            parent: parent.to_string(),
            after: after.to_string(),
            revision,
        })
    }

    /// Whether this token belongs to the request presenting it.
    ///
    /// A token is a position inside one particular walk. Presented against
    /// another collection or another parent it does not mean "start there" — it
    /// means the caller has two walks confused, and answering would hand back
    /// objects they never asked for, in an order that looks deliberate.
    pub fn check(&self, kind: &str, parent: &str) -> ApiResult<()> {
        if self.kind != kind || self.parent != parent {
            return Err(ApiError::invalid(format!(
                "this page token was issued for {}/{} and was presented for {parent}/{kind}; \
                 start the list again",
                self.parent, self.kind
            )));
        }
        Ok(())
    }
}

/// How much of a collection a caller wants, and from where.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Paging {
    /// Resolved: absent or zero means [`DEFAULT_PAGE_SIZE`], anything above
    /// [`MAX_PAGE_SIZE`] is capped to it.
    pub size: Option<usize>,
    pub token: Option<PageToken>,
}

impl Paging {
    /// Everything, in one answer.
    ///
    /// Kept for the callers that genuinely want a whole collection — a
    /// controller reconciling it, an agent building its cache. They are not the
    /// callers this module is about, and making them page would be making them
    /// re-implement the loop.
    pub fn unpaged() -> Self {
        Self::default()
    }

    pub fn of(size: usize) -> Self {
        Self {
            size: Some(size),
            token: None,
        }
    }

    /// The page size actually in force.
    pub fn resolved_size(&self) -> usize {
        match self.size {
            None | Some(0) => DEFAULT_PAGE_SIZE,
            Some(n) => n.min(MAX_PAGE_SIZE),
        }
    }

    /// Whether the caller asked for a page at all.
    ///
    /// `size: None` with no token is the whole collection, which is what every
    /// internal caller wants and what the REST surface turns into a real page.
    pub fn is_paged(&self) -> bool {
        self.size.is_some() || self.token.is_some()
    }
}
