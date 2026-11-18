//! One shared HTTP client: keeps connections to Discord open between requests
//! (no new TLS handshake each time) and never waits for an answer forever.

use std::{sync::OnceLock, time::Duration};

pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(15))
            .max_idle_connections_per_host(8)
            .build()
    })
}
