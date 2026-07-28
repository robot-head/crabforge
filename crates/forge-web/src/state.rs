//! What the web handlers need, and who is asking.

use std::{path::PathBuf, sync::Arc};

use forge_auth::{Scope, Scopes};
use forge_command::CommandService;
use forge_store::Store;
use time::Duration;
use tokio::sync::watch;

/// How long a session lasts without being refreshed.
pub const SESSION_LIFETIME: Duration = Duration::days(14);

/// How long a write waits for its own projection before answering anyway.
pub const PROJECTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

pub struct WebState {
    pub store: Arc<Store>,
    pub commands: Option<Arc<CommandService>>,
    pub bootstrap: String,
    /// Where per-repository git caches live. Disposable.
    pub cache_root: PathBuf,
    /// Keys CSRF tokens. Never leaves the process.
    pub csrf_secret: Vec<u8>,
    /// Set the `Secure` flag on cookies. Off for local development, where
    /// there is no TLS and the alternative is being unable to log in.
    pub secure_cookies: bool,
    /// Projection progress per topic, for reading a write back after making it.
    pub applied: std::collections::HashMap<String, watch::Receiver<i64>>,
}

impl WebState {
    /// Wait for a projector to catch up with an offset a command committed at.
    ///
    /// Returns false on timeout. The write still landed — only the projection
    /// is behind — so a caller redirects anyway rather than reporting failure.
    pub async fn await_projection(&self, topic: &str, offset: Option<i64>) -> bool {
        let Some(offset) = offset else {
            return true;
        };
        let Some(applied) = self.applied.get(topic) else {
            return false;
        };
        forge_projector_wait(applied.clone(), offset, PROJECTION_BUDGET).await
    }
}

/// Wait for a watch channel to reach `offset`.
///
/// Duplicated from `forge-projector` rather than depending on it: the web crate
/// needs the projector's *output*, not its machinery, and a dependency edge
/// from the presentation layer to the projection layer would invite one going
/// the other way.
async fn forge_projector_wait(
    mut applied: watch::Receiver<i64>,
    offset: i64,
    within: std::time::Duration,
) -> bool {
    if *applied.borrow() >= offset {
        return true;
    }
    tokio::time::timeout(within, async {
        while applied.changed().await.is_ok() {
            if *applied.borrow() >= offset {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false)
}

/// Who is making a request.
#[derive(Debug, Clone)]
pub struct Viewer {
    pub user_id: String,
    pub username: String,
    /// The session's stored hash, which CSRF tokens are derived from.
    pub session_hash: String,
    pub scopes: Scopes,
}

impl Viewer {
    /// Whether this viewer may act at `needed`.
    pub fn allows(&self, needed: Scope) -> bool {
        self.scopes.allows(needed)
    }
}

/// A viewer, or nobody.
///
/// Most pages render for both, so the common case is `Option<Viewer>` rather
/// than a redirect: a signed-out visitor should see a public repository, not a
/// login page.
pub type MaybeViewer = Option<Viewer>;

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn a_session_lasts_two_weeks() {
        // Long enough not to be a nuisance, short enough that a stolen cookie
        // is not indefinite.
        check!(SESSION_LIFETIME == Duration::days(14));
    }

    #[test]
    fn a_browser_viewer_holds_every_scope() {
        // Someone signed in through the browser is acting as themselves; scopes
        // exist to limit *tokens*, which are handed to other software.
        let viewer = Viewer {
            user_id: "u".into(),
            username: "octocat".into(),
            session_hash: "h".into(),
            scopes: Scopes::new(Scope::all()),
        };
        for scope in Scope::all() {
            check!(viewer.allows(scope));
        }
    }
}
