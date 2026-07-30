//! What the broker was formatted to do.
//!
//! Most of crabka is configurable at run time. A few things are not: KIP-584
//! feature levels are written into the metadata log when the log directory is
//! formatted, and `share.version` — the one gating the KIP-932 share groups the
//! CI work queue rides on — is among them. A broker formatted without
//! `--feature share.version=1` cannot be reconfigured into serving them; the
//! only recovery is to format again, which discards the log.
//!
//! Left undetected that is a deployment mistake reported as a runtime error:
//! everything else the forge does works, and the first symptom is a runner
//! failing to join its group with a bare `broker error_code`, possibly long
//! after whoever ran `crabka format` has moved on.
//!
//! KIP-584 puts the answer on the wire. `ApiVersionsResponse` carries the
//! finalized feature levels next to the per-API version ranges, so one
//! handshake with the broker settles it. The admin client surfaces only the
//! version ranges, which is why this reads the response itself rather than
//! going through [`crabka_client_admin`].

use std::collections::BTreeMap;

use crabka_client_core::{Client, ClientError};
use crabka_protocol::owned::api_versions_request::ApiVersionsRequest;

/// The KIP-932 feature gating share-group membership.
pub const SHARE_VERSION: &str = "share.version";

/// The `share.version` level at which share groups are usable (KIP-932 GA).
pub const SHARE_GROUPS_LEVEL: i16 = 1;

#[derive(Debug, thiserror::Error)]
pub enum FeatureError {
    #[error("asking the broker which features it has: {0}")]
    Client(#[from] ClientError),
    #[error("the broker rejected the feature query with error_code {0}")]
    Server(i16),
}

/// The feature levels a broker reports as finalized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrokerFeatures {
    levels: BTreeMap<String, i16>,
}

impl BrokerFeatures {
    /// Ask the broker at `bootstrap` what it has.
    pub async fn probe(bootstrap: &str) -> Result<Self, FeatureError> {
        let client = Client::builder()
            .bootstrap(bootstrap)
            .client_id("forge-features")
            .build()
            .await?;

        // v3+ of this request carries the KIP-511 client-software fields, and
        // the broker rejects the call outright if either is empty or fails its
        // `[a-zA-Z0-9](?:[a-zA-Z0-9\-.]*[a-zA-Z0-9])?` check. `Default` leaves
        // both empty, so they are filled in here rather than spread.
        let response = client
            .send(ApiVersionsRequest {
                client_software_name: "crabforge".into(),
                client_software_version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            })
            .await?;

        if response.error_code != 0 {
            return Err(FeatureError::Server(response.error_code));
        }

        Ok(Self::from_levels(
            response
                .finalized_features
                .into_iter()
                .map(|f| (f.name, f.max_version_level)),
        ))
    }

    /// Build from `(feature, level)` pairs. For tests and for callers that
    /// already hold a decoded response.
    pub fn from_levels(levels: impl IntoIterator<Item = (String, i16)>) -> Self {
        Self {
            levels: levels.into_iter().collect(),
        }
    }

    /// The finalized level of `feature`.
    ///
    /// An unlisted feature is level 0, not "unknown". That is the KIP-584
    /// meaning — level 0 is the disabled state, and a broker only lists a
    /// feature once a level has been finalized for it — so absence and
    /// `share.version=0` are the same fact and callers should not have to
    /// distinguish them.
    #[must_use]
    pub fn level(&self, feature: &str) -> i16 {
        self.levels.get(feature).copied().unwrap_or(0)
    }

    /// Whether this broker will serve share groups.
    #[must_use]
    pub fn share_groups(&self) -> bool {
        self.level(SHARE_VERSION) >= SHARE_GROUPS_LEVEL
    }

    /// Every finalized feature, for a diagnostic that wants to show its work.
    pub fn iter(&self) -> impl Iterator<Item = (&str, i16)> {
        self.levels
            .iter()
            .map(|(name, level)| (name.as_str(), *level))
    }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn features(pairs: &[(&str, i16)]) -> BrokerFeatures {
        BrokerFeatures::from_levels(pairs.iter().map(|(n, l)| ((*n).to_string(), *l)))
    }

    #[test]
    fn an_unlisted_feature_is_disabled_rather_than_unknown() {
        let f = features(&[("metadata.version", 25)]);
        check!(f.level(SHARE_VERSION) == 0);
        check!(!f.share_groups());
    }

    #[test]
    fn share_groups_need_level_one() {
        check!(!features(&[(SHARE_VERSION, 0)]).share_groups());
        check!(features(&[(SHARE_VERSION, 1)]).share_groups());
        // A level above the one we know about still serves share groups: the
        // forge must not refuse to run against a broker newer than itself.
        check!(features(&[(SHARE_VERSION, 2)]).share_groups());
    }
}
