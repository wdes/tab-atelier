// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Spending a smaller model when the plan is nearly spent.
//!
//! The alternative to degrading is failing. Once the shared five-hour window
//! is nearly exhausted, every remaining call is a choice about who gets cut
//! off — and a fleet worker grinding a backlog on Opus is a much worse use of
//! the last few percent than a person waiting on an answer.
//!
//! So above a threshold the proxy rewrites the requested model down a chain,
//! and below it changes nothing. This is graceful degradation in the `QoS`
//! sense: reduce the service rather than drop the request.
//!
//! # Two rules that keep this honest
//!
//! **Never silently.** A downgrade is reported back in an
//! `x-tab-atelier-proxy-fallback` header naming both models. A caller that
//! receives a cheaper answer than it asked for is entitled to know — silently
//! swapping models would make results irreproducible and the proxy untrustworthy.
//!
//! **Never upward.** The chain only ever moves toward cheaper. A request that
//! already asks for Haiku is left alone; the proxy has no business deciding
//! someone needs a *better* model than they asked for.

/// Cheapest-last chain. A model not in this list is never rewritten — an
/// unknown name is far more likely to be something deliberate than something
/// we should second-guess.
const CHAIN: &[&str] = &["claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"];

/// Utilisation of the shared five-hour window above which downgrading starts.
///
/// 0.85 rather than 0.95: by the time a window is 95% spent, the remaining
/// headroom is minutes, and starting to conserve then is too late to keep an
/// interactive session alive through the tail.
pub const DEGRADE_ABOVE: f64 = 0.85;

/// Above this, go straight to the cheapest model in the chain rather than one
/// step down. At 95% the question is no longer which model, it is whether
/// anything gets served at all.
pub const FLOOR_ABOVE: f64 = 0.95;

/// What a model should be replaced with, if anything.
///
/// `weight` is the caller's `QoS` weight: heavier accounts are degraded later,
/// on the same reasoning that gives them a larger share. `None` means leave
/// the request exactly as it is.
#[must_use]
pub fn downgrade(model: &str, utilization: Option<f64>, weight: u32) -> Option<&'static str> {
    let util = utilization?;
    // A heavier account tolerates more pressure before it is degraded, scaled
    // gently so that even weight 10 still degrades before the window is spent.
    let headroom = f64::from(weight.clamp(1, 10) - 1) * 0.01;
    let degrade_above = DEGRADE_ABOVE + headroom;
    if util < degrade_above {
        return None;
    }
    let pos = CHAIN.iter().position(|m| *m == model)?;
    let target = if util >= FLOOR_ABOVE {
        CHAIN.len() - 1
    } else {
        (pos + 1).min(CHAIN.len() - 1)
    };
    // Never upward, and never a no-op rewrite.
    (target > pos).then(|| CHAIN[target])
}

/// Rewrite the `model` field of a request body, returning the new body and
/// what was swapped.
///
/// Returns `None` when nothing should change, so the common path does not copy
/// the body at all.
#[must_use]
pub fn rewrite_body(body: &[u8], utilization: Option<f64>, weight: u32) -> Option<(Vec<u8>, String, String)> {
    // Cheap reject before parsing: most bodies will not be rewritten, and
    // parsing a large request just to discover that is wasted work.
    utilization.filter(|u| *u >= DEGRADE_ABOVE)?;
    let mut v = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let model = v.get("model")?.as_str()?.to_owned();
    let target = downgrade(&model, utilization, weight)?;
    *v.get_mut("model")? = serde_json::Value::String(target.to_owned());
    let out = serde_json::to_vec(&v).ok()?;
    Some((out, model, target.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_window_changes_nothing() {
        assert_eq!(downgrade("claude-opus-5", Some(0.10), 1), None);
        assert_eq!(downgrade("claude-opus-5", Some(0.84), 1), None);
        // No reading at all is not an excuse to degrade: unknown must never
        // read as "nearly exhausted".
        assert_eq!(downgrade("claude-opus-5", None, 1), None);
    }

    #[test]
    fn a_tight_window_steps_one_model_down() {
        assert_eq!(downgrade("claude-opus-5", Some(0.86), 1), Some("claude-sonnet-5"));
        assert_eq!(downgrade("claude-sonnet-5", Some(0.86), 1), Some("claude-haiku-4-5"));
    }

    /// Past the floor the question is no longer which model but whether
    /// anything is served, so it goes straight to the cheapest.
    #[test]
    fn a_nearly_spent_window_goes_straight_to_the_cheapest() {
        assert_eq!(downgrade("claude-opus-5", Some(0.97), 1), Some("claude-haiku-4-5"));
    }

    /// The chain only moves one way. Nothing here should ever decide someone
    /// needs a better model than they asked for.
    #[test]
    fn the_cheapest_model_is_never_rewritten_and_never_upgraded() {
        assert_eq!(downgrade("claude-haiku-4-5", Some(0.99), 1), None);
        // An unknown model is left alone: far more likely deliberate than
        // something to second-guess.
        assert_eq!(downgrade("some-finetune-of-ours", Some(0.99), 1), None);
        assert_eq!(downgrade("", Some(0.99), 1), None);
    }

    /// Heavier accounts are degraded later, for the same reason they get a
    /// bigger share — but weight is not an exemption.
    #[test]
    fn weight_buys_headroom_but_not_immunity() {
        assert_eq!(downgrade("claude-opus-5", Some(0.86), 1), Some("claude-sonnet-5"));
        assert_eq!(
            downgrade("claude-opus-5", Some(0.86), 5),
            None,
            "weight 5 tolerates more"
        );
        // Even the heaviest weight degrades before the window is actually spent.
        assert!(downgrade("claude-opus-5", Some(0.96), 10).is_some());
    }

    #[test]
    fn the_body_is_rewritten_in_place_and_reports_the_swap() {
        let body = br#"{"model":"claude-opus-5","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}"#;
        let (out, from, to) = rewrite_body(body, Some(0.9), 1).expect("should downgrade");
        assert_eq!((from.as_str(), to.as_str()), ("claude-opus-5", "claude-sonnet-5"));
        let v: serde_json::Value = serde_json::from_slice(&out).expect("valid json out");
        assert_eq!(v["model"], "claude-sonnet-5");
        // Everything else must survive untouched — this rewrites one field,
        // it does not reconstruct the request.
        assert_eq!(v["max_tokens"], 100);
        assert_eq!(v["messages"][0]["content"], "hi");
    }

    #[test]
    fn nothing_is_rewritten_when_it_should_not_be() {
        let body = br#"{"model":"claude-opus-5","max_tokens":100}"#;
        assert!(rewrite_body(body, Some(0.2), 1).is_none(), "healthy window");
        assert!(rewrite_body(body, None, 1).is_none(), "no reading");
        assert!(rewrite_body(b"not json", Some(0.99), 1).is_none(), "unparseable body");
        assert!(
            rewrite_body(br#"{"max_tokens":1}"#, Some(0.99), 1).is_none(),
            "no model field to rewrite"
        );
    }
}
