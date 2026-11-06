//! By hand only: real Tailscale SSH to a real host.
//!
//! ```text
//! ROOST_SSH_LIVE_TARGET=charliek@popos \
//!   cargo test -p roost-ssh --test live_test -- --ignored --nocapture
//! ```
//!
//! Trusts the host key it is shown (printing it), then runs
//! `session.identify` — read-only — through `client-bridge`. Set
//! `ROOST_SSH_LIVE_PIN` to check against a known fingerprint instead.

mod support;

use roost_client::dial::{DialError, Dialer};
use roost_ssh::{HostKeyPolicy, HostTarget, TailscaleSshDialer};
use support::identify;

#[tokio::test]
#[ignore = "needs ROOST_SSH_LIVE_TARGET and a Tailscale SSH host"]
async fn identify_over_tailscale_ssh() {
    let target = std::env::var("ROOST_SSH_LIVE_TARGET").expect("set ROOST_SSH_LIVE_TARGET");
    let target = HostTarget::parse(&target, "").expect("a [user@]host[:port] target");
    let pinned = std::env::var("ROOST_SSH_LIVE_PIN").ok();
    let dialer = TailscaleSshDialer::new(
        target,
        HostKeyPolicy {
            pinned: pinned.clone(),
        },
    );

    let duplex = match dialer.dial().await {
        Err(DialError::UntrustedHostKey { host, fingerprint }) if pinned.is_none() => {
            println!("host key for {host}: {fingerprint} (trusting it for this run)");
            dialer.set_host_key_policy(HostKeyPolicy {
                pinned: Some(fingerprint),
            });
            dialer.dial().await
        }
        other => other,
    }
    .unwrap_or_else(|error| panic!("dial {}: {error}", dialer.describe()));

    match identify(duplex).await {
        Ok(identity) => println!("session.identify over {}: {identity:?}", dialer.describe()),
        Err(error) => panic!(
            "identify over {} failed: {error}; last remote error: {:?}",
            dialer.describe(),
            dialer.last_remote_error()
        ),
    }
}
