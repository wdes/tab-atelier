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
    /// `rerouted` (same class, different provider), `degraded` (cheaper
    /// class) or `pinned` (the account's model override). Distinct because
    /// they mean different things to whoever reads the header: the first two
    /// preserved or traded away the answer, the third says who chose it.
    pub reason: Option<&'static str>,
}

/// The host state a routing decision reads.
///
/// Both fields are injected rather than looked up, so the decision is a function
/// of what is true rather than of which machine the caller happens to run on:
/// `get` is the environment an [`crate::provider::Auth::ApiKeyEnv`] provider
/// resolves its key in, and `subscription_present` is whether this host holds
/// the Claude credential file the subscription hop authenticates with — the same
/// question, asked of the one credential that is a file instead of a variable.
///
/// Grouped into one value because they are one idea (can this host
/// authenticate?) and because seven arguments is already as many as a routing
/// call should take.
#[derive(Debug, Clone, Copy)]
pub struct Host<G> {
    /// Looks up an environment variable, for `ApiKeyEnv` providers.
    pub get: G,
    /// Whether the subscription's credential file exists.
    pub subscription_present: bool,
}

impl<G: Fn(&str) -> Option<String>> Host<G> {
    /// Whether this host can authenticate to `provider` at all.
    #[must_use]
    pub fn authenticates(&self, provider: &Provider) -> bool {
        provider.credential_ready_with(&self.get, self.subscription_present)
    }
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
pub fn choose<G: Fn(&str) -> Option<String> + Copy>(
    registry: &Registry,
    requested: &str,
    pinned: Option<&str>,
    kind: crate::classifier::Kind,
    health: &dyn Fn(&str) -> Health,
    host: Host<G>,
    now: u64,
) -> Option<Route> {
    // No mapping layer: names are what the caller asked for. A per-person model
    // mapping is applied by the caller AFTER this returns, because it can select
    // on the answer — see `relay::pick_route`. Keeping it out of here is what
    // makes the two scopes one mechanism instead of two.
    let target = requested.to_owned();
    let mapped_from: Option<String> = None;

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
            if let Some((p, m)) = pick_in(registry, c, health, host, now, pinned, allow_strained) {
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

/// Resolve an account's model pin to a destination.
///
/// Distinct from [`choose`] in the one way that matters: `model_id` is
/// resolved EXACTLY. `choose` reads a requested name as a class hint and is
/// free to answer with any model of that class — which is right for a client
/// naming a model it merely prefers, and wrong for an operator pinning one
/// person to one model. Someone who pins `gpt-5.6-luna` means Luna, not
/// "whichever fast model is cheapest right now", so a pin nothing serves
/// returns `None` and the caller reports 503 rather than substituting.
///
/// **A model pin outranks a provider pin.** `provider` is tried first and then
/// dropped: choosing a model chooses the hop that serves it. The two pins
/// disagreeing is not a misconfiguration to punish — it is an operator saying
/// "this person on this provider, except for this model", which is the only way
/// to say that at all.
///
/// This used to pass `provider` straight through as a filter, so a model pin
/// naming anything the pinned provider did not serve left no candidates and
/// every request 503'd. Two doc comments asserted the opposite — here and at
/// `Registry::providers_serving` — which is how it went unnoticed: the contract
/// was written down and never implemented.
///
/// `changed_from` is left for the caller to fill: only it knows the name the
/// client actually asked for.
#[must_use]
pub fn choose_exact<G: Fn(&str) -> Option<String> + Copy>(
    registry: &Registry,
    model_id: &str,
    provider: Option<&str>,
    kind: crate::classifier::Kind,
    health: &dyn Fn(&str) -> Health,
    host: Host<G>,
    now: u64,
) -> Option<Route> {
    // The pinned provider first, so an account whose provider does serve the
    // pinned model stays where it was put — the pin still contains everything
    // it can contain.
    pick_exact(registry, model_id, health, host, now, provider)
        // Then anywhere, because the model is the more specific instruction.
        .or_else(|| pick_exact(registry, model_id, health, host, now, None))
        .map(|(p, m)| Route {
            provider_id: p.id.clone(),
            model_id: m.id.clone(),
            class: m.class,
            kind,
            changed_from: None,
            reason: Some("pinned"),
        })
}

/// A provider that serves this exact model id, healthy first.
fn pick_exact<'a, G: Fn(&str) -> Option<String> + Copy>(
    registry: &'a Registry,
    model_id: &str,
    health: &dyn Fn(&str) -> Health,
    host: Host<G>,
    now: u64,
    pinned: Option<&str>,
) -> Option<(&'a Provider, &'a Model)> {
    let candidates = registry.providers_serving(model_id, host.get, host.subscription_present, now, pinned);
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

fn pick_in<'a, G: Fn(&str) -> Option<String> + Copy>(
    registry: &'a Registry,
    class: Class,
    health: &dyn Fn(&str) -> Health,
    host: Host<G>,
    now: u64,
    pinned: Option<&str>,
    allow_strained: bool,
) -> Option<(&'a Provider, &'a Model)> {
    registry
        .candidates_pinned(class, host.get, host.subscription_present, now, pinned)
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

    /// The host state most tests run against: every key present, and the
    /// subscription's credential file in place.
    ///
    /// A `fn` pointer rather than a closure so the value stays `Copy` without
    /// borrowing anything, and one helper so the subscription's presence is
    /// stated once here rather than at each call site — a test that means to
    /// exercise a *missing* credential should say so, not inherit it.
    fn host() -> Host<fn(&str) -> Option<String>> {
        Host {
            get: env_with_key,
            subscription_present: true,
        }
    }

    /// The same host with the subscription's credential file absent — the state
    /// a proxy is in before anyone has logged in.
    fn host_without_subscription() -> Host<fn(&str) -> Option<String>> {
        Host {
            get: env_with_key,
            subscription_present: false,
        }
    }

    /// Anthropic plus a second provider serving the same classes from its own
    /// quota — the arrangement the whole feature exists for.
    fn two_providers() -> Registry {
        let mut r = Registry::default();
        r.providers.push(Provider {
            user_id: None,
            peak: None,
            server_tools: None,
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
                    price: None,
                    deprecated: false,
                    note: None,
                    context_window: crate::provider::DEFAULT_CONTEXT_WINDOW,
                },
                Model {
                    id: "bedrock.haiku".to_owned(),
                    class: Class::Fast,
                    relative_cost: 2,
                    price: None,
                    deprecated: false,
                    note: None,
                    context_window: crate::provider::DEFAULT_CONTEXT_WINDOW,
                },
            ],
        });
        r
    }

    #[test]
    fn a_healthy_subscription_serves_what_was_asked_for() {
        let r = two_providers();
        let route = choose(&r, "claude-opus-5-5", None, classifier::Kind::Work, &healthy, host(), 0).expect("a route");
        assert_eq!(route.provider_id, "anthropic");
        assert_eq!(route.model_id, "claude-opus-5-5");
        assert_eq!(route.changed_from, None, "nothing to report when nothing changed");
        assert_eq!(route.reason, None);
    }

    /// A subscription with no credential file on this host is not a destination,
    /// and nothing else is configured — so the answer is `None`, which the caller
    /// turns into a 503. Before this was checked, the route was chosen and the
    /// failure surfaced later as a read error from the egress, once per poll.
    #[test]
    fn a_subscription_with_no_credential_file_routes_nowhere() {
        let r = Registry::default(); // subscription only
        assert_eq!(
            choose(
                &r,
                "claude-opus-5-5",
                None,
                classifier::Kind::Work,
                &healthy,
                host_without_subscription(),
                0
            ),
            None,
            "a provider that cannot authenticate is not a route"
        );
    }

    /// The credential check must not be so broad that a *second* provider with a
    /// key is refused because the subscription's file happens to be absent: the
    /// work has somewhere else to go and must go there.
    #[test]
    fn a_keyed_provider_still_serves_when_the_subscription_has_no_credential() {
        let r = two_providers();
        let route = choose(
            &r,
            "claude-opus-5-5",
            None,
            classifier::Kind::Work,
            &healthy,
            host_without_subscription(),
            0,
        )
        .expect("bedrock can serve this");
        assert_eq!(
            route.provider_id, "bedrock",
            "the usable provider serves, rather than the request failing outright"
        );
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
            "claude-opus-5-5",
            None,
            classifier::Kind::Work,
            &squeezed,
            host(),
            0,
        )
        .expect("a route");
        assert_eq!(route.provider_id, "bedrock", "the work should move, not shrink");
        assert_eq!(route.class, Class::Heavy, "and stay at the same class");
        assert_eq!(route.reason, Some("rerouted"));
        assert_eq!(route.changed_from.as_deref(), Some("claude-opus-5-5"));
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
            choose(&r, "claude-opus-5-5", None, classifier::Kind::Work, &blocked, host(), 0),
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
            "claude-opus-5-5",
            None,
            classifier::Kind::Work,
            &bedrock_blocked,
            host(),
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
            "claude-opus-5-5",
            None,
            classifier::Kind::Work,
            &strained,
            host(),
            0,
        )
        .expect("a route");
        assert_eq!(
            route.class,
            Class::Heavy,
            "the last tenth of a paid plan is still usable"
        );
        assert_eq!(route.model_id, "claude-opus-5-5");
        assert_eq!(route.reason, None);
    }

    /// Never upward. Idle capacity in an expensive class is not a reason to
    /// spend it on work that asked for something cheap.
    #[test]
    fn a_cheap_request_is_never_promoted() {
        let r = two_providers();
        let route = choose(
            &r,
            "claude-haiku-5-5",
            None,
            classifier::Kind::Work,
            &healthy,
            host(),
            0,
        )
        .expect("a route");
        assert_eq!(route.class, Class::Fast);
        assert_ne!(route.model_id, "claude-opus-5-5");
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
            host(),
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

    /// `choose` does not rewrite names: a per-person mapping is applied after it.
    ///
    /// The plain `Registry.mappings` table that used to live here was removed —
    /// mapping is per account now (`Account.routes`), and it is applied by
    /// `relay::pick_route` on the ANSWER this returns, because a rule may select
    /// on the provider the request was heading for. So this function must leave
    /// the name alone, and this test is what stops a second rewrite layer growing
    /// back in a place where it would run before the pin.
    #[test]
    fn choose_does_not_rewrite_the_requested_name() {
        let r = with_deepseek();

        let route = choose(&r, "claude-opus-5-5", None, classifier::Kind::Work, &healthy, host(), 0).expect("a route");

        assert_ne!(
            route.model_id, "deepseek-flash",
            "no mapping layer here: the name is the caller's",
        );
        assert_eq!(route.changed_from, None, "and nothing was changed on the way through");
    }
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
            "claude-opus-5-5",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            host(),
            0,
        ) {
            assert_eq!(rt.provider_id, "deepseek", "the pin filters, it does not hint");
        }

        // Pinned to Anthropic: DeepSeek is never chosen, whatever it costs.
        let route = choose(
            &r,
            "claude-opus-5-5",
            Some("anthropic"),
            classifier::Kind::Work,
            &healthy,
            host(),
            0,
        )
        .expect("a route");
        assert_eq!(route.provider_id, "anthropic");
    }

    /// A client asking for a retired model name still gets served.
    ///
    /// The catalogue moves when a vendor ships a generation — `claude-opus-5`
    /// became `claude-opus-5-5` — but clients in the wild keep asking for what
    /// they were configured with, sometimes for months. `class_of` falls back to
    /// a family substring for exactly this, so an old name is read as a class
    /// hint and answered with the current model of that class.
    ///
    /// Asserted because the rename relies on it: without this fallback, every
    /// client pinned to a previous generation would break the day the catalogue
    /// was updated, and the failure would look like a routing outage.
    #[test]
    fn a_retired_model_name_is_still_routed_by_its_family() {
        let r = with_deepseek();

        let route = choose(&r, "claude-opus-5", None, classifier::Kind::Work, &healthy, host(), 0)
            .expect("a retired name still names a family");

        assert_eq!(route.class, crate::provider::Class::Heavy, "opus is still Heavy");
        assert_eq!(
            route.model_id, "claude-opus-5-5",
            "answered with the current Heavy model"
        );
    }

    /// Fable is Heavy, which it has to be read as from its name alone.
    ///
    /// `class_of` matches a family substring when the id is not in the
    /// catalogue, and `fable` was missing from that table — so a request for
    /// any Fable id the registry did not list fell through to `None`, which
    /// `choose` reads as Balanced. The most capable model would have been
    /// routed as a mid-tier one.
    #[test]
    fn fable_is_read_as_the_heavy_class_it_is() {
        let r = with_deepseek();

        assert_eq!(
            r.class_of("claude-fable-5-1-20260601"),
            Some(crate::provider::Class::Heavy),
            "an unlisted Fable id is still Heavy",
        );
    }

    // --- choose_exact: the model pin ---------------------------------------
    //
    // This function had NO tests before the bug below was found, which is how
    // the bug shipped: the behaviour it got wrong was documented in two places
    // and checked in none.

    /// A model pin reaches its model even when the account is pinned elsewhere.
    ///
    /// **This is the regression test for the bug.** `choose_exact` passed the
    /// provider pin into the candidate filter, so the provider was narrowed
    /// before the model was resolved: pin someone to Anthropic and to
    /// `deepseek-flash` and nothing served the model, so every request 503'd.
    /// Two doc comments promised the opposite — "a model pin outranks a
    /// provider pin" — which is why nobody looked.
    ///
    /// The exception is deliberate and is the whole point: it is the only way
    /// to say "this person on this provider, except for this model".
    #[test]
    fn a_model_pin_outranks_the_provider_pin_it_disagrees_with() {
        let r = with_deepseek();

        let route = choose_exact(
            &r,
            "deepseek-flash",
            Some("anthropic"),
            classifier::Kind::Work,
            &healthy,
            host(),
            0,
        )
        .expect("the model pin names a model, and something serves it");

        assert_eq!(route.provider_id, "deepseek", "the model chose the hop that serves it");
        assert_eq!(route.model_id, "deepseek-flash");
        assert_eq!(route.reason, Some("pinned"));
    }

    /// The provider pin still wins when it CAN serve the pinned model.
    ///
    /// Dropping the pin entirely would be the opposite error: an account pinned
    /// to a reseller that serves the model must stay on the reseller, not be
    /// re-decided by preference order. The pin is tried first and only then
    /// dropped.
    #[test]
    fn a_provider_that_serves_the_pinned_model_keeps_it() {
        let mut r = with_deepseek();
        // A second provider serving the very same id, cheaper and preferred,
        // so preference order would move the request if the pin were ignored.
        let mut rival = crate::provider::Preset::Deepseek.provider(std::path::Path::new("/tmp"));
        rival.id = "reseller".to_owned();
        rival.auth = crate::provider::Auth::ApiKeyEnv { var: "K".to_owned() };
        rival.preference = -10;
        r.providers.push(rival);

        let route = choose_exact(
            &r,
            "deepseek-flash",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            host(),
            0,
        )
        .expect("a route");

        assert_eq!(
            route.provider_id, "deepseek",
            "the pin is honoured when it can be; only an unservable pin is dropped"
        );
    }

    /// A model pin nothing serves is a visible failure, not a substitution.
    ///
    /// The point of an exact pin is that it is exact. Falling back to the class
    /// ladder here would quietly give the operator a model they did not choose,
    /// which is the behaviour `choose` exists for and `choose_exact` exists to
    /// avoid.
    #[test]
    fn a_model_pin_nothing_serves_yields_no_route() {
        let r = with_deepseek();

        assert!(
            choose_exact(
                &r,
                "no-such-model",
                Some("anthropic"),
                classifier::Kind::Work,
                &healthy,
                host(),
                0,
            )
            .is_none(),
            "a 503 the operator can see, not a quiet substitution"
        );
    }

    /// A pinned model on a blocked provider does not degrade.
    ///
    /// `choose` would step down a class; `choose_exact` must not. An operator
    /// who pinned a model wants that model or an error — and the error is what
    /// tells them the provider is in trouble.
    #[test]
    fn a_pinned_model_on_a_blocked_provider_does_not_degrade() {
        let r = with_deepseek();
        let blocked = |id: &str| Health {
            utilization: None,
            backoff_secs: if id == "deepseek" { 60 } else { 0 },
        };

        assert!(
            choose_exact(&r, "deepseek-flash", None, classifier::Kind::Work, &blocked, host(), 0,).is_none(),
            "the pin is a pin: no ladder, no other provider's model"
        );
    }

    /// A pin to something that is not configured must fail visibly.
    #[test]
    fn a_pin_to_nothing_yields_no_route_rather_than_a_quiet_substitution() {
        let r = with_deepseek();
        assert_eq!(
            choose(
                &r,
                "claude-sonnet-5-5",
                Some("typo-not-a-provider"),
                classifier::Kind::Work,
                &healthy,
                host(),
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
            "claude-sonnet-5-5",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            host(),
            1_789_016_400,
        );
        assert_eq!(off.map(|rt| rt.provider_id), Some("deepseek".to_owned()));

        // At peak flash is 30, so the subscription would win — if it were
        // allowed. The pin still decides here, which is the point: peak moves
        // the price, the pin moves the traffic.
        let peak = choose(
            &r,
            "claude-sonnet-5-5",
            Some("deepseek"),
            classifier::Kind::Work,
            &healthy,
            host(),
            1_789_005_600,
        );
        assert_eq!(peak.map(|rt| rt.provider_id), Some("deepseek".to_owned()));

        // With no pin, the ordering is what changes.
        let unpinned_peak = choose(
            &r,
            "claude-sonnet-5-5",
            None,
            classifier::Kind::Work,
            &healthy,
            host(),
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
        let empty = Registry { providers: vec![] };
        assert_eq!(
            choose(
                &empty,
                "claude-opus-5-5",
                None,
                classifier::Kind::Work,
                &healthy,
                host(),
                0
            ),
            None
        );
    }
}
