//! Sending a delivery, and deciding what to do when it fails.

use std::time::Duration;

use forge_store::WebhookRecord;

/// How long a receiver has to respond.
///
/// Short on purpose. A slow endpoint should not hold a delivery worker, and a
/// receiver that needs more than this to acknowledge a webhook is doing work it
/// ought to do asynchronously.
const TIMEOUT: Duration = Duration::from_secs(10);

/// How many times a delivery is attempted before it is given up on.
pub const MAX_ATTEMPTS: i64 = 5;

/// Largest body sent. Bigger than any forge event, small enough that a receiver
/// cannot be used as a bandwidth amplifier.
const MAX_BODY: usize = 1024 * 1024;

/// What happened to one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The receiver accepted it.
    Delivered { status: u16, duration_ms: i64 },
    /// It failed, and trying again might work.
    Retry { reason: String, status: Option<u16> },
    /// It failed in a way that will not improve.
    Permanent { reason: String, status: Option<u16> },
}

impl DeliveryOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Delivered { .. })
    }

    /// The stored status word.
    pub fn status_word(&self, attempt: i64) -> &'static str {
        match self {
            Self::Delivered { .. } => "delivered",
            Self::Permanent { .. } => "dead",
            Self::Retry { .. } if attempt >= MAX_ATTEMPTS => "dead",
            Self::Retry { .. } => "failed",
        }
    }
}

/// How long to wait before attempt `n`.
///
/// Exponential with a ceiling. A receiver that is down stays down for a while,
/// and hammering it neither helps them nor us — but the ceiling matters too: an
/// unbounded backoff means a delivery that lands hours after the event, which
/// is worse than one that is given up on.
pub fn backoff(attempt: i64) -> Duration {
    const CEILING: u64 = 300;
    let seconds = 2u64.saturating_pow(attempt.clamp(0, 16) as u32);
    Duration::from_secs(seconds.min(CEILING))
}

/// The body and headers of a delivery.
pub struct Payload {
    pub event_id: String,
    pub event_type: String,
    pub body: Vec<u8>,
    /// CloudEvents attributes in HTTP spelling, carried through from the event.
    pub ce_headers: Vec<(String, String)>,
}

/// One delivery to make.
pub struct Delivery {
    pub webhook: WebhookRecord,
    pub payload: Payload,
    pub attempt: i64,
}

/// Sends deliveries.
pub struct Deliverer {
    client: reqwest::Client,
    /// Whether a target on a private or loopback address may be called.
    ///
    /// Off by default, because a webhook pointed at the forge's own network is
    /// a request-forgery primitive handed to whoever can configure one. On for
    /// a single-user forge on a laptop, where the whole point may be to call a
    /// service running beside it — the same trade as `Secure` on cookies.
    allow_private_targets: bool,
}

impl Default for Deliverer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deliverer {
    pub fn new() -> Self {
        Self::with_private_targets(false)
    }

    /// A deliverer that will call private and loopback addresses.
    ///
    /// For development and for a forge whose operator has decided its users are
    /// trusted. Never appropriate on a forge open to strangers.
    pub fn with_private_targets(allow_private_targets: bool) -> Self {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // Redirects are not followed. A receiver that redirects is either
            // misconfigured or walking us somewhere the target check already
            // refused, and following one would evade that check entirely.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("crabforge-webhooks/1")
            .build()
            .expect("a client with static settings always builds");
        Self {
            client,
            allow_private_targets,
        }
    }

    /// Attempt one delivery.
    pub async fn send(&self, delivery: &Delivery) -> DeliveryOutcome {
        if delivery.payload.body.len() > MAX_BODY {
            return DeliveryOutcome::Permanent {
                reason: "payload is too large to deliver".into(),
                status: None,
            };
        }

        // Resolved and checked immediately before the request, not only when
        // the webhook was saved: DNS can be repointed in between.
        if !self.allow_private_targets
            && let Err(e) = target::resolve_and_check(&delivery.webhook.url).await
        {
            return DeliveryOutcome::Permanent {
                reason: e.to_string(),
                status: None,
            };
        }

        let signature = crate::sign(&delivery.webhook.secret, &delivery.payload.body);
        let started = std::time::Instant::now();

        let mut request = self
            .client
            .post(&delivery.webhook.url)
            .header("content-type", "application/json")
            .header(crate::SIGNATURE_HEADER, signature)
            .header("X-Forge-Event", &delivery.payload.event_type)
            // The CloudEvents id doubles as the delivery id, so a receiver can
            // deduplicate a retry without keeping its own bookkeeping.
            .header("X-Forge-Delivery", &delivery.payload.event_id);
        for (name, value) in &delivery.payload.ce_headers {
            request = request.header(name, value);
        }

        let response = match request.body(delivery.payload.body.clone()).send().await {
            Ok(response) => response,
            Err(e) => {
                return DeliveryOutcome::Retry {
                    reason: e.to_string(),
                    status: None,
                };
            }
        };

        let status = response.status().as_u16();
        let duration_ms = started.elapsed().as_millis() as i64;

        if response.status().is_success() {
            DeliveryOutcome::Delivered {
                status,
                duration_ms,
            }
        } else if response.status().is_server_error() || status == 429 {
            // The receiver is struggling, not refusing.
            DeliveryOutcome::Retry {
                reason: format!("receiver returned {status}"),
                status: Some(status),
            }
        } else {
            // A 4xx means the request itself is unacceptable. Sending it again
            // unchanged will produce the same answer.
            DeliveryOutcome::Permanent {
                reason: format!("receiver returned {status}"),
                status: Some(status),
            }
        }
    }
}

use crate::target;

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        check!(backoff(1) < backoff(2));
        check!(backoff(2) < backoff(3));
        // Bounded: a delivery arriving hours late is worse than one abandoned.
        check!(backoff(50) == Duration::from_secs(300));
        check!(
            backoff(9) == Duration::from_secs(300),
            "the ceiling is reached, not the clamp"
        );
    }

    #[test]
    fn a_server_error_is_retried_and_a_client_error_is_not() {
        // The distinction that decides whether a broken integration retries
        // forever or gives up.
        let retry = DeliveryOutcome::Retry {
            reason: "500".into(),
            status: Some(500),
        };
        check!(retry.status_word(1) == "failed");
        check!(
            retry.status_word(MAX_ATTEMPTS) == "dead",
            "eventually given up on"
        );

        let permanent = DeliveryOutcome::Permanent {
            reason: "404".into(),
            status: Some(404),
        };
        check!(
            permanent.status_word(1) == "dead",
            "no point retrying a 404"
        );
    }

    #[test]
    fn a_delivered_outcome_is_success() {
        let ok = DeliveryOutcome::Delivered {
            status: 200,
            duration_ms: 12,
        };
        check!(ok.is_success());
        check!(ok.status_word(1) == "delivered");
        let retry = DeliveryOutcome::Retry {
            reason: String::new(),
            status: None,
        };
        check!(!retry.is_success());
    }
}
