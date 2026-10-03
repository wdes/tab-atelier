// SPDX-License-Identifier: MPL-2.0

//! The reachability probe.
//!
//! The desktop app pings this before a tab starts, to find out whether the
//! relay is up. That ping used to be an ordinary request — authenticated and
//! routed to a provider — which meant a proxy that was merely slow to reach an
//! upstream looked unreachable, and the app refused to start tabs that would
//! have worked. This answers first, and answers without touching anything.
//!
//! It is a route rather than a check in front of every route because that is
//! what it is: one path, answered from memory, reachable without credentials.
//! Putting it in the route table also means Rocket's own router decides it, so
//! it cannot fall behind the paths the relay actually serves.

use crate::transport::{Reply, json};

/// Answer the probe.
///
/// `HEAD` is the form a health checker normally uses, and gets no body; `GET`
/// gets `{}` so a client that parses JSON is not handed an empty string. Both
/// are 200 whenever the process is alive, which is the only question being
/// asked — whether an upstream is reachable is deliberately not part of it.
#[must_use]
pub fn hello(method: &str) -> Reply {
    let body = if method.eq_ignore_ascii_case("HEAD") { "" } else { "{}" };
    json(200, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::ReplyBody;

    fn body_of(reply: &Reply) -> String {
        match &reply.body {
            ReplyBody::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            ReplyBody::Stream(_) => unreachable!("the probe is two bytes"),
        }
    }

    #[test]
    fn a_head_probe_gets_no_body_and_a_get_probe_gets_json() {
        // A body on a HEAD response is a protocol violation some clients
        // reject outright, and the health checker is exactly the client that
        // sends HEAD.
        assert_eq!(body_of(&hello("HEAD")), "");
        assert_eq!(body_of(&hello("GET")), "{}");
    }

    #[test]
    fn the_probe_answers_whatever_the_process_is_doing_upstream() {
        // The whole point: this must not depend on an upstream being reachable,
        // or a slow provider still reads as a dead proxy.
        assert_eq!(hello("GET").status, 200);
    }
}
