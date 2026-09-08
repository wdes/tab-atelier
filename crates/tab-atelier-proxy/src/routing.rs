// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Choosing where a request goes.
//!
//! The caller does not pick the model. It asks for one, and that name says
//! what KIND of work this is ([`Class`]); the proxy decides which
//! `(provider, model)` actually serves it, from whatever has capacity at that
//! moment. A developer cannot know that the five-hour window is 97% spent, or
//! that Bedrock is answering while the subscription is not. The proxy can.
//!
//! # Two moves, in order
//!
//! **Reroute first.** Same class, different provider. Bedrock and Vertex serve
//! the same models from different quota pools, so a saturated subscription is
//! a reason to move the work, not to make it worse. The answer is unchanged.
//!
//! **Degrade only if there is nowhere left.** [`Class::cheaper`] down the
//! ladder, which trades quality for getting an answer at all. This is the last
//! resort and it used to be the FIRST — the old `fallback` module stepped down
//! Anthropic's ladder because a single provider left nothing else to try.
//!
//! # What is never done
//!
//! Never upward: a request for something fast is not silently promoted to a
//! model that costs twenty times more, whatever is idle.
//!
//! Never silently: the chosen route travels back on the response, so a caller
//! that got something other than what it asked for can see that, and a
//! recorded transcript says which provider produced it.

use crate::provider::{Class, Model, Provider, Registry};

/// How busy a provider is, as far as the proxy can tell.
///
/// Deliberately a plain struct the caller fills in: pressure comes from the
/// plan's own report for the subscription, and from 429s for everyone else,
/// and routing should not care which.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Health {
    /// 0.0–1.0 of the provider's window, if known. `None` means no signal,
    /// which is treated as healthy — inventing scarcity nobody reported is
    /// how a router talks itself out of using a working provider.
    pub utilization: Option<f64>,
    /// Refusing traffic until this many seconds from now.
    pub backoff_secs: u64,
}

impl Health {
    /// Upstream said stop, so nothing goes here at all.
    #[must_use]
    pub const fn blocked(&self) -> bool {
        self.backoff_secs > 0
    }

    /// Past this, a provider is kept for work that has nowhere else to go.
    ///
    /// Not a hard cut: a provider at 90% is still serving, and refusing to use
    /// it while it works would waste the last tenth of a plan somebody paid
    /// for. It is a preference, applied only when an alternative exists.
    #[must_use]
    pub fn strained(&self) -> bool {
        self.utilization.is_some_and(|u| u >= STRAINED_ABOVE)
    }
}

/// Utilisation above which a provider stops being the first choice.
pub const STRAINED_ABOVE: f64 = 0.85;

/// Where a request is going, and why it is not where it was headed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub provider_id: String,
    pub model_id: String,
    pub class: Class,
    /// Set when this is not what the caller asked for.
    pub changed_from: Option<String>,
    /// `rerouted` (same class, different provider) or `degraded` (cheaper
    /// class). Distinct because they mean different things to whoever reads
    /// the header: one preserved the answer, the other did not.
    pub reason: Option<&'static str>,
}

/// Pick a destination for a request that asked for `requested`.
///
/// `health` answers for a provider id. Returns `None` only when nothing is
/// configured that could serve the work at all — a caller should treat that as
/// 503 rather than guessing.
#[must_use]
pub fn choose(
    registry: &Registry,
    requested: &str,
    health: &dyn Fn(&str) -> Health,
    env: impl Fn(&str) -> Option<String> + Copy,
) -> Option<Route> {
    // An unrecognised name is treated as the middle of the road. Refusing it
    // would break a client asking for a model we simply have not listed, and
    // assuming Heavy would hand it the most expensive thing we have.
    let asked_class = registry.class_of(requested).unwrap_or(Class::Balanced);

    // First pass: the class the caller's request implies, healthy providers
    // only. Second: the same class including strained ones — a strained
    // provider still works, and using it beats degrading the answer. Only then
    // step down a class.
    let mut class = Some(asked_class);
    while let Some(c) = class {
        for allow_strained in [false, true] {
            if let Some((p, m)) = pick_in(registry, c, health, env, allow_strained) {
                let changed = m.id != requested;
                return Some(Route {
                    provider_id: p.id.clone(),
                    model_id: m.id.clone(),
                    class: c,
                    changed_from: changed.then(|| requested.to_owned()),
                    reason: if !changed {
                        None
                    } else if c == asked_class {
                        Some("rerouted")
                    } else {
                        Some("degraded")
                    },
                });
            }
        }
        class = c.cheaper();
    }
    None
}

fn pick_in<'a>(
    registry: &'a Registry,
    class: Class,
    health: &dyn Fn(&str) -> Health,
    env: impl Fn(&str) -> Option<String> + Copy,
    allow_strained: bool,
) -> Option<(&'a Provider, &'a Model)> {
    registry.candidates_with(class, env).into_iter().find(|(p, _)| {
        let h = health(&p.id);
        // A blocked provider is never used: upstream has said so outright,
        // and sending anyway is what turns one 429 into a sustained one.
        !h.blocked() && (allow_strained || !h.strained())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Auth, Model, Provider, Wire};

    fn healthy(_: &str) -> Health {
        Health::default()
    }
    /// Every provider's key is present, so credential readiness never silently
    /// removes a candidate a test meant to exercise. `Option` because that is
    /// the shape `choose` takes for an environment lookup.
    #[allow(clippy::unnecessary_wraps)]
    fn env_with_key(_: &str) -> Option<String> {
        Some("k".to_owned())
    }

    /// Anthropic plus a second provider serving the same classes from its own
    /// quota — the arrangement the whole feature exists for.
    fn two_providers() -> Registry {
        let mut r = Registry::default();
        r.providers.push(Provider {
            id: "bedrock".to_owned(),
            wire: Wire::Anthropic,
            base_url: "https://bedrock.example".to_owned(),
            auth: Auth::ApiKeyEnv { var: "K".to_owned() },
            preference: 1,
            enabled: true,
            models: vec![
                Model {
                    id: "bedrock.opus".to_owned(),
                    class: Class::Heavy,
                    relative_cost: 30,
                },
                Model {
                    id: "bedrock.haiku".to_owned(),
                    class: Class::Fast,
                    relative_cost: 2,
                },
            ],
        });
        r
    }

    #[test]
    fn a_healthy_subscription_serves_what_was_asked_for() {
        let r = two_providers();
        let route = choose(&r, "claude-opus-5", &healthy, env_with_key).expect("a route");
        assert_eq!(route.provider_id, "anthropic");
        assert_eq!(route.model_id, "claude-opus-5");
        assert_eq!(route.changed_from, None, "nothing to report when nothing changed");
        assert_eq!(route.reason, None);
    }

    /// The headline behaviour: out of capacity moves the work, it does not
    /// make the answer worse.
    #[test]
    fn a_saturated_provider_reroutes_to_another_at_the_same_class() {
        let r = two_providers();
        let squeezed = |id: &str| {
            if id == "anthropic" {
                Health {
                    utilization: Some(0.97),
                    backoff_secs: 0,
                }
            } else {
                Health::default()
            }
        };
        let route = choose(&r, "claude-opus-5", &squeezed, env_with_key).expect("a route");
        assert_eq!(route.provider_id, "bedrock", "the work should move, not shrink");
        assert_eq!(route.class, Class::Heavy, "and stay at the same class");
        assert_eq!(route.reason, Some("rerouted"));
        assert_eq!(route.changed_from.as_deref(), Some("claude-opus-5"));
    }

    /// Only when there is nowhere left does the answer get cheaper.
    #[test]
    fn degrading_happens_only_when_no_provider_can_serve_the_class() {
        // Subscription alone, and it is blocked outright.
        let r = Registry::default();
        let blocked = |_: &str| Health {
            utilization: None,
            backoff_secs: 30,
        };
        assert_eq!(
            choose(&r, "claude-opus-5", &blocked, env_with_key),
            None,
            "a blocked provider serves nothing, at any class"
        );

        // Now a registry whose only heavy model is on a blocked provider, but
        // whose cheaper classes live elsewhere.
        let mut r = two_providers();
        r.providers[0].models.retain(|m| m.class != Class::Heavy);
        let bedrock_blocked = |id: &str| {
            if id == "bedrock" {
                Health {
                    utilization: None,
                    backoff_secs: 60,
                }
            } else {
                Health::default()
            }
        };
        let route = choose(&r, "claude-opus-5", &bedrock_blocked, env_with_key).expect("a route");
        assert_eq!(
            route.class,
            Class::Balanced,
            "stepped down only after finding nothing heavy"
        );
        assert_eq!(route.provider_id, "anthropic");
        assert_eq!(route.reason, Some("degraded"));
    }

    /// A strained provider is still a working provider. Using it beats
    /// degrading, so it is preferred over a cheaper class — just not over a
    /// healthy peer.
    #[test]
    fn a_strained_provider_is_used_before_the_answer_is_degraded() {
        let r = Registry::default(); // subscription only
        let strained = |_: &str| Health {
            utilization: Some(0.93),
            backoff_secs: 0,
        };
        let route = choose(&r, "claude-opus-5", &strained, env_with_key).expect("a route");
        assert_eq!(
            route.class,
            Class::Heavy,
            "the last tenth of a paid plan is still usable"
        );
        assert_eq!(route.model_id, "claude-opus-5");
        assert_eq!(route.reason, None);
    }

    /// Never upward. Idle capacity in an expensive class is not a reason to
    /// spend it on work that asked for something cheap.
    #[test]
    fn a_cheap_request_is_never_promoted() {
        let r = two_providers();
        let route = choose(&r, "claude-haiku-4-5-20251001", &healthy, env_with_key).expect("a route");
        assert_eq!(route.class, Class::Fast);
        assert_ne!(route.model_id, "claude-opus-5");
    }

    #[test]
    fn an_unknown_model_is_treated_as_ordinary_work() {
        let r = Registry::default();
        let route = choose(&r, "some-model-we-never-listed", &healthy, env_with_key).expect("a route");
        // Not Heavy — an unrecognised name must not buy the most expensive
        // thing available — and not a refusal either.
        assert_eq!(route.class, Class::Balanced);
        assert_eq!(route.reason, Some("rerouted"));
    }

    #[test]
    fn nothing_configured_means_no_route_rather_than_a_guess() {
        let empty = Registry { providers: vec![] };
        assert_eq!(choose(&empty, "claude-opus-5", &healthy, env_with_key), None);
    }
}
