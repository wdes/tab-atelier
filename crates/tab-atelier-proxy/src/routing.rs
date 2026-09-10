// SPDX-License-Identifier: MPL-2.0

//! Choosing where a request goes.
//!
//! The caller does not pick the model. It asks for one, and that name says
//! what KIND of work this is ([`Class`]); the proxy decides which
//! `(provider, model)` actually serves it, from whatever has capacity at that
//! moment. A developer cannot know that the five-hour window is 97% spent, or
//! that Bedrock is answering while the subscription is not. The proxy can.
//!
//! # Three moves, in order
//!
//! **Mapping first.** If the operator has written `opus → deepseek-flash`,
//! that is a decision and it is honoured verbatim when the named model exists
//! somewhere usable. See [`crate::provider::Mapping`].
//!
//! **Reroute second.** Same class, different provider. Bedrock and Vertex serve
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
//! recorded transcript says which provider produced it. `mapped`, `rerouted`
//! and `degraded` are distinct because they mean different things — a mapping
//! was somebody's decision, a reroute preserved the answer, a degrade did not.
//!
//! # Pinning
//!
//! An account may be pinned to one provider ([`crate::users::Account::provider`]).
//! That is a statement about where someone's work is allowed to go, so it
//! filters candidates rather than merely preferring one, and an empty set is a
//! 503 — visible — rather than a quiet fall back to the provider the operator
//! was trying to keep them off.

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
    /// Whether this is the conversation or the auto-mode classifier. Carried
    /// here because the routing decision is what makes it matter: it is the
    /// difference between a destination chosen for work and one chosen for the
    /// gate that guards work.
    pub kind: crate::classifier::Kind,
    /// Set when this is not what the caller asked for.
    pub changed_from: Option<String>,
    /// `rerouted` (same class, different provider) or `degraded` (cheaper
    /// class). Distinct because they mean different things to whoever reads
    /// the header: one preserved the answer, the other did not.
    pub reason: Option<&'static str>,
}

/// Pick a destination for a request that asked for `requested`.
///
/// `health` answers for a provider id. `pinned` is the account's provider, if
/// it has one — see [`Registry::candidates_pinned`] for why that filters
/// rather than hints. `kind` is the conversation or the auto-mode classifier;
/// see [`crate::classifier::Kind::retargets_by_mapping`] for why only the
/// former may be rewritten by a mapping. Returns `None` only when nothing is
/// configured that could serve the work at all — a caller should treat that as
/// 503 rather than guessing.
#[must_use]
pub fn choose(
    registry: &Registry,
    requested: &str,
    pinned: Option<&str>,
    kind: crate::classifier::Kind,
    health: &dyn Fn(&str) -> Health,
    env: impl Fn(&str) -> Option<String> + Copy,
    now: u64,
) -> Option<Route> {
    // A mapping rewrites the NAME and nothing else. Health, preference and the
    // degrade ladder all still apply to whatever it resolves to, so an
    // operator who writes `opus → deepseek-flash` gets DeepSeek when DeepSeek
    // is usable and the normal ladder when it is not — rather than a hard pin
    // that turns into an outage the first time the far end rate-limits.
    //
    // Held to work, though: the classifier judges whether work may run, and
    // "save money on the conversation" is not a statement about who adjudicates
    // `rm -rf`. So the classifier is routed on the name it asked for. The pin
    // and the ladder below still apply, so it goes somewhere fast and cheap.
    let (target, mapped_from) = if kind.retargets_by_mapping() {
        registry.resolve(requested)
    } else {
        (requested.to_owned(), None)
    };

    // An explicit mapping to a model that exactly ONE provider serves is a
    // decision about destination, not just a rename: `deepseek-flash` exists
    // only at DeepSeek, so preference order must not send it to Anthropic.
    // Tried before the class ladder, because it is the most specific answer
    // available.
    if mapped_from.is_some()
        && let Some((p, m)) = pick_exact(registry, &target, health, env, now, pinned)
    {
        return Some(Route {
            provider_id: p.id.clone(),
            model_id: m.id.clone(),
            class: m.class,
            kind,
            changed_from: mapped_from,
            reason: Some("mapped"),
        });
    }

    // An unrecognised name is treated as the middle of the road. Refusing it
    // would break a client asking for a model we simply have not listed, and
    // assuming Heavy would hand it the most expensive thing we have.
    let asked_class = registry.class_of(&target).unwrap_or(Class::Balanced);
    // Nothing was mapped, but the name may still have been rewritten by a
    // chain we then failed to place — report that rather than losing it.
    let changed_from = mapped_from.or_else(|| (target != requested).then(|| requested.to_owned()));

    // First pass: the class the caller's request implies, healthy providers
    // only. Second: the same class including strained ones — a strained
    // provider still works, and using it beats degrading the answer. Only then
    // step down a class.
    let mut class = Some(asked_class);
    while let Some(c) = class {
        for allow_strained in [false, true] {
            if let Some((p, m)) = pick_in(registry, c, health, env, now, pinned, allow_strained) {
                let changed = m.id != target;
                // Moved, not cloned: the loop returns on the first hit, so
                // this value is consumed exactly once.
                let from = if changed {
                    changed_from.or_else(|| Some(target.clone()))
                } else {
                    changed_from
                };
                let reason = if from.is_none() && !changed {
                    None
                } else if c == asked_class {
                    Some("rerouted")
                } else {
                    Some("degraded")
                };
                return Some(Route {
                    provider_id: p.id.clone(),
                    model_id: m.id.clone(),
                    class: c,
                    kind,
                    changed_from: from,
                    reason,
                });
            }
        }
        class = c.cheaper();
    }
    None
}

/// A provider that serves this exact model id, healthy first.
fn pick_exact<'a>(
    registry: &'a Registry,
    model_id: &str,
    health: &dyn Fn(&str) -> Health,
    env: impl Fn(&str) -> Option<String> + Copy,
    now: u64,
    pinned: Option<&str>,
) -> Option<(&'a Provider, &'a Model)> {
    let candidates = registry.providers_serving(model_id, env, now, pinned);
    // Blocked is absolute: upstream said stop, and sending anyway is what
    // turns one 429 into a sustained one. So a blocked provider is not a
    // candidate even when a mapping named it — the whole reason a mapping is a
    // rewrite and not a pin is that it must survive the far end refusing.
    let usable: Vec<_> = candidates.iter().filter(|(p, _)| !health(&p.id).blocked()).collect();
    // Strained is a preference, not a bar: a mapping named this model, and
    // using a busy provider that serves it beats quietly serving something
    // else.
    usable
        .iter()
        .find(|(p, _)| !health(&p.id).strained())
        .or_else(|| usable.first())
        .copied()
        .copied()
}

fn pick_in<'a>(
    registry: &'a Registry,
    class: Class,
    health: &dyn Fn(&str) -> Health,
    env: impl Fn(&str) -> Option<String> + Copy,
    now: u64,
    pinned: Option<&str>,
    allow_strained: bool,
) -> Option<(&'a Provider, &'a Model)> {
    registry
        .candidates_pinned(class, env, now, pinned)
        .into_iter()
        .find(|(p, _)| {
            let h = health(&p.id);
            // A blocked provider is never used: upstream has said so outright,
            // and sending anyway is what turns one 429 into a sustained one.
            !h.blocked() && (allow_strained || !h.strained())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier;
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
            peak: None,
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
                    deprecated: false,
                    note: None,
                },
                Model {
                    id: "bedrock.haiku".to_owned(),
                    class: Class::Fast,
                    relative_cost: 2,
                    deprecated: false,
                    note: None,
                },
            ],
        });
        r
    }

    #[test]
    fn a_healthy_subscription_serves_what_was_asked_for() {
        let r = two_providers();
        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        )
        .expect("a route");
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
        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &squeezed,
            env_with_key,
            0,
        )
        .expect("a route");
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
            choose(
                &r,
                "claude-opus-5",
                None,
                classifier::Kind::Work,
                &blocked,
                env_with_key,
                0
            ),
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
        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &bedrock_blocked,
            env_with_key,
            0,
        )
        .expect("a route");
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
        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &strained,
            env_with_key,
            0,
        )
        .expect("a route");
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
        let route = choose(
            &r,
            "claude-haiku-4-5-20251001",
            None,
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        )
        .expect("a route");
        assert_eq!(route.class, Class::Fast);
        assert_ne!(route.model_id, "claude-opus-5");
    }

    #[test]
    fn an_unknown_model_is_treated_as_ordinary_work() {
        let r = Registry::default();
        let route = choose(
            &r,
            "some-model-we-never-listed",
            None,
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        )
        .expect("a route");
        // Not Heavy — an unrecognised name must not buy the most expensive
        // thing available — and not a refusal either.
        assert_eq!(route.class, Class::Balanced);
        assert_eq!(route.reason, Some("rerouted"));
    }

    /// A registry where `DeepSeek` is configured and usable, with its preset's
    /// models and peak schedule.
    ///
    /// The preset ships a file credential, which would be filtered out here
    /// for having no key on disk — correctly, and it is worth noticing that
    /// this helper has to say otherwise. A provider that cannot authenticate
    /// is not a candidate at any price.
    fn with_deepseek() -> Registry {
        let mut r = Registry::default();
        let mut ds = crate::provider::Preset::Deepseek.provider(std::path::Path::new("/tmp"));
        ds.auth = crate::provider::Auth::ApiKeyEnv { var: "K".to_owned() };
        r.providers.push(ds);
        r
    }

    /// The case the whole mapping feature exists for: a client asks for a
    /// Claude model name, and the answer is a model only another provider has.
    #[test]
    fn a_mapping_to_another_providers_model_routes_there() {
        let mut r = with_deepseek();
        r.set_mapping("claude-opus-5", "deepseek-flash", None);

        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        )
        .expect("a route");
        // Not Anthropic, even though it has preference 0 and serves a model of
        // the same class. `deepseek-flash` exists in exactly one place, so the
        // mapping named a destination, not just a rename.
        assert_eq!(route.provider_id, "deepseek");
        assert_eq!(route.model_id, "deepseek-flash");
        assert_eq!(route.reason, Some("mapped"));
        assert_eq!(route.changed_from.as_deref(), Some("claude-opus-5"));
    }

    /// Mapping claude to claude: a deliberate downgrade, same provider.
    #[test]
    fn a_mapping_within_one_provider_is_honoured_even_though_it_costs_less() {
        let mut r = Registry::default();
        r.set_mapping("claude-opus-5", "claude-sonnet-5", Some("cost control".to_owned()));

        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        )
        .expect("a route");
        assert_eq!(route.provider_id, "anthropic");
        assert_eq!(route.model_id, "claude-sonnet-5");
        assert_eq!(route.class, Class::Balanced);
        assert_eq!(route.reason, Some("mapped"));
    }

    /// A mapping is a decision, not a hard pin: when the mapped destination is
    /// unreachable, routing still has somewhere to go. A pin would turn the
    /// first rate-limit at the far end into an outage.
    #[test]
    fn a_mapping_does_not_become_a_pin_that_survives_an_outage() {
        let mut r = with_deepseek();
        r.set_mapping("claude-opus-5", "deepseek-flash", None);

        // DeepSeek is refusing traffic outright.
        let no_deepseek = |id: &str| Health {
            utilization: None,
            backoff_secs: if id == "deepseek" { 60 } else { 0 },
        };
        let route = choose(
            &r,
            "claude-opus-5",
            None,
            classifier::Kind::Work,
            &no_deepseek,
            env_with_key,
            0,
        )
        .expect("a route");
        assert_eq!(
            route.provider_id, "anthropic",
            "the work still gets done; the mapping was a preference for DeepSeek, and it is unusable"
        );
    }

    /// Pinning is a statement about where someone's work is allowed to go.
    #[test]
    fn a_pinned_account_only_ever_reaches_its_provider() {
        let r = with_deepseek();
        // Pinned to DeepSeek: even a heavy request, which DeepSeek has no
        // current model for, does not fall back to the subscription.
        // Either DeepSeek or nothing at all — never a quiet substitution. The
        // `None` case is the honest answer: a 503 the operator can see, rather
        // than a fall back to the very provider they were keeping this person
        // off.
        if let Some(rt) = choose(
            &r,
            "claude-opus-5",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        ) {
            assert_eq!(rt.provider_id, "deepseek", "the pin filters, it does not hint");
        }

        // Pinned to Anthropic: DeepSeek is never chosen, whatever it costs.
        let route = choose(
            &r,
            "claude-opus-5",
            Some("anthropic"),
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            0,
        )
        .expect("a route");
        assert_eq!(route.provider_id, "anthropic");
    }

    /// A pin to something that is not configured must fail visibly.
    #[test]
    fn a_pin_to_nothing_yields_no_route_rather_than_a_quiet_substitution() {
        let r = with_deepseek();
        assert_eq!(
            choose(
                &r,
                "claude-sonnet-5",
                Some("typo-not-a-provider"),
                classifier::Kind::Work,
                &healthy,
                env_with_key,
                0
            ),
            None,
            "a pinned account whose provider is gone must 503, not silently bill someone else"
        );
    }

    /// Peak pricing is a property of the provider, evaluated at the moment of
    /// the request — so it can reorder two providers serving the same class.
    #[test]
    fn peak_pricing_is_evaluated_per_provider_at_the_time_of_the_request() {
        let mut r = Registry::default();
        // Make the subscription's only balanced model expensive enough that
        // the two are within a factor of the multiplier, so peak decides.
        for m in &mut r.providers[0].models {
            if m.class == Class::Balanced {
                m.relative_cost = 20;
            }
        }
        let mut ds = crate::provider::Preset::Deepseek.provider(std::path::Path::new("/tmp"));
        ds.auth = crate::provider::Auth::ApiKeyEnv { var: "K".to_owned() };
        r.providers.push(ds);

        // Off-peak: DeepSeek's flash (15) beats the subscription (20).
        let off = choose(
            &r,
            "claude-sonnet-5",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            1_789_016_400,
        );
        assert_eq!(off.map(|rt| rt.provider_id), Some("deepseek".to_owned()));

        // At peak flash is 30, so the subscription would win — if it were
        // allowed. The pin still decides here, which is the point: peak moves
        // the price, the pin moves the traffic.
        let peak = choose(
            &r,
            "claude-sonnet-5",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            1_789_005_600,
        );
        assert_eq!(peak.map(|rt| rt.provider_id), Some("deepseek".to_owned()));

        // With no pin, the ordering is what changes.
        let unpinned_peak = choose(
            &r,
            "claude-sonnet-5",
            None,
            classifier::Kind::Work,
            &healthy,
            env_with_key,
            1_789_005_600,
        )
        .expect("a route");
        assert_eq!(
            unpinned_peak.provider_id, "anthropic",
            "at peak, a provider whose price doubles should lose a close call"
        );
    }

    #[test]
    fn nothing_configured_means_no_route_rather_than_a_guess() {
        let empty = Registry {
            providers: vec![],
            mappings: vec![],
        };
        assert_eq!(
            choose(
                &empty,
                "claude-opus-5",
                None,
                classifier::Kind::Work,
                &healthy,
                env_with_key,
                0
            ),
            None
        );
    }
}
