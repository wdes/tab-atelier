// SPDX-License-Identifier: MPL-2.0

//! The price list: the models this proxy bills for, and the rates it bills.
//!
//! One path serves two audiences, which is why this resource is a union rather
//! than either shape alone.
//!
//! - A client asking *which models exist* — Claude Code, on startup — reads
//!   `data[].{type,id,display_name}`, the shape the Anthropic `GET /v1/models`
//!   returns.
//! - `catbus-agent` asking *what a turn costs*, so it can show a price against
//!   its own usage, reads `data[].{id,amounts}`.
//!
//! Both describe the same models, so one entry carries both sets of fields and
//! neither client has to guess.
//!
//! # Why it is served here rather than relayed
//!
//! The rates are not obtainable from the provider. `DeepSeek`'s published list
//! carries `id`, `object` and `owned_by` and nothing to scale a token count by —
//! its prices live on a separate page a client cannot call — and the surface
//! this hop is configured against (`wire = "anthropic"`, a `base_url` ending in
//! `/anthropic`) does not serve the route at all. Forwarding it returned 404, so
//! every turn the agent made was counted and none was priced. The rates are in
//! the registry, so the answer is built from there.
//!
//! # What a rate means
//!
//! `Price` is micro-USD per 1M tokens, off-peak, and every figure here has been
//! through `Provider::price_at` for the instant the list was built — so it is
//! the rate this proxy would bill *right now*, peak multiplier included. That
//! makes the list a snapshot, which is the honest shape for one: a client
//! fetches it once, while `peak` moves the same rate by a factor at fixed UTC
//! hours, so a session spanning a window boundary is priced at whichever side it
//! was opened on. `as_of` is carried so a quote taken beside a boundary can be
//! told apart from one taken away from it. The authoritative figure stays the
//! proxy's own usage report, which prices hour by hour.
//!
//! # Which models appear
//!
//! Only those a client can be billed for at a rate that is true: the hop is
//! enabled (a disabled one serves nothing), the model is not withdrawn (routing
//! skips it, and requests to it are served by a different model at *that*
//! model's price), and a rate exists at all (absent means "no figure exists",
//! never "costs nothing").
//!
//! No `created_at`: the registry does not record when a model was added, and a
//! date invented to fill the field would be worse than its absence. No
//! pagination fields either — the list is complete, and a cursor for a next page
//! that does not exist would be noise.

use std::collections::HashSet;

use serde::Serialize;

use crate::provider::{Model, Provider, Registry};

/// The unit every rate is quoted per.
///
/// One constant rather than a per-entry field, because a list whose entries
/// disagreed about their unit could not be summed: a client scales a token
/// count by whatever it finds here.
const UNIT_TOKENS: u32 = 1_000_000;

/// The currency the rates are in. The registry holds one.
const CURRENCY: &str = "USD";

/// The whole list.
#[derive(Serialize)]
pub(crate) struct PriceListResource {
    /// Always `"list"`, the envelope the provider's own list uses.
    pub object: &'static str,
    /// Unix seconds the rates were read for. Neither client consults it; it is
    /// kept so a quote taken beside a peak boundary can be told apart from one
    /// taken away from it.
    pub as_of: u64,
    pub data: Vec<ModelEntryResource>,
}

/// One model: how to name it, and what it costs.
#[derive(Serialize)]
pub(crate) struct ModelEntryResource {
    /// `"model"`, as the Anthropic wire spells this field.
    #[serde(rename = "type")]
    pub model_type: &'static str,
    /// The id the relay reports as having served a turn. This is the registry
    /// id — what `x-tab-atelier-proxy-route` carries — so it is the id a client
    /// should send and the id it looks a finished turn up by.
    pub id: String,
    /// The display name, for the model picker.
    ///
    /// Two spellings of one string, because the two clients disagree on the
    /// field and neither tolerates the other's: the Anthropic wire calls it
    /// `display_name`, and the price reader calls it `name` (falling back to
    /// `id`, so its absence would be survivable but its presence is the
    /// documented contract). Emitting both is cheaper than a client that
    /// renders a blank name or a model it cannot match.
    pub name: String,
    pub display_name: String,
    pub amounts: Vec<AmountResource>,
}

/// One rate, in the shape a client scales a token count by.
#[derive(Serialize)]
pub(crate) struct AmountResource {
    pub currency: &'static str,
    pub unit_tokens: u32,
    /// One of `input`, `output`, `cache_read`, `cache_write`.
    pub kind: &'static str,
    /// `currency` per `unit_tokens`.
    pub price: f64,
}

impl PriceListResource {
    /// Build the list, quoting each rate as of `now`.
    ///
    /// `now` is a parameter rather than a reading of the clock so the peak
    /// window can be exercised either side of its boundary.
    #[must_use]
    pub(crate) fn of(registry: &Registry, now: u64) -> Self {
        let mut data = Vec::new();
        let mut seen = HashSet::new();
        for provider in registry.providers.iter().filter(|p| p.enabled) {
            for model in &provider.models {
                // First writer wins. Two enabled hops can carry the same model
                // id, and a client can only hold one figure for it; the
                // registry's order is the order the operator sees, and roughly
                // the routing preference, so the first entry is the one to keep.
                if !seen.insert(model.id.as_str()) {
                    continue;
                }
                if let Some(entry) = ModelEntryResource::of(provider, model, now) {
                    data.push(entry);
                }
            }
        }
        Self {
            object: "list",
            as_of: now,
            data,
        }
    }
}

impl ModelEntryResource {
    /// One entry, or `None` when this model has no rate to publish.
    fn of(provider: &Provider, model: &Model, now: u64) -> Option<Self> {
        if model.deprecated {
            return None;
        }
        // The same call the bill makes, peak included, so what a client shows
        // is what the ledger would charge for these tokens.
        let price = provider.price_at(model, now)?;
        // Cache *writes* are billed at the cache-hit rate: the usage fold sums
        // the two counts into one and prices it with `Price::cache_hit`. Quoting
        // a write at any other rate would disagree with the bill for the same
        // tokens.
        let amounts = vec![
            AmountResource::of("cache_read", price.cache_hit),
            AmountResource::of("cache_write", price.cache_hit),
            AmountResource::of("input", price.input),
            AmountResource::of("output", price.output),
        ];
        Some(Self {
            model_type: "model",
            id: model.id.clone(),
            name: model.id.clone(),
            display_name: model.id.clone(),
            amounts,
        })
    }
}

impl AmountResource {
    /// One rate. `micro` is micro-USD per 1M — the unit the registry stores —
    /// and the wire wants the currency unit itself, so this is the single place
    /// the division happens.
    fn of(kind: &'static str, micro: u32) -> Self {
        Self {
            currency: CURRENCY,
            unit_tokens: UNIT_TOKENS,
            kind,
            price: f64::from(micro) / f64::from(UNIT_TOKENS),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::Value;

    use super::*;
    use crate::provider::Preset;

    /// A Monday inside the peak window and one outside it, as unix seconds.
    /// Peak is the reason `as_of` is on the wire at all, so both sides of it are
    /// exercised rather than one.
    const PEAK_MONDAY_02Z: u64 = 1_767_578_400;
    const OFFPEAK_MONDAY_12Z: u64 = 1_767_614_400;

    fn deepseek() -> Registry {
        Registry {
            providers: vec![Preset::Deepseek.provider(Path::new("/tmp/tab-atelier-test"))],
            mappings: Vec::new(),
        }
    }

    fn list(now: u64) -> Value {
        let registry = deepseek();
        let list = PriceListResource::of(&registry, now);
        serde_json::to_value(&list).expect("the list serialises")
    }

    fn entry(list: &Value, id: &str) -> Value {
        let data = list["data"].as_array().expect("data is an array");
        data.iter()
            .find(|m| m["id"] == id)
            .unwrap_or_else(|| panic!("{id} is listed: {list}"))
            .clone()
    }

    fn amount(entry: &Value, kind: &str) -> f64 {
        let amounts = entry["amounts"].as_array().expect("amounts is an array");
        amounts
            .iter()
            .find(|a| a["kind"] == kind)
            .unwrap_or_else(|| panic!("{kind} is quoted: {entry}"))["price"]
            .as_f64()
            .expect("price is a number")
    }

    /// Prices reach the wire as `f64`, so they are compared the way the rest of
    /// the crate compares money: at a tolerance far tighter than any real rate,
    /// and loose enough to ignore the last bit of the division by `UNIT_TOKENS`.
    fn assert_price(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
    }

    /// The figures are `DeepSeek`'s published off-peak rates for Flash. Pinned
    /// rather than derived from the registry, because the point of the list is
    /// that the number is right, not that it round-trips.
    #[test]
    fn the_rates_are_the_published_ones() {
        let flash = entry(&list(OFFPEAK_MONDAY_12Z), "deepseek-flash");
        assert_price(amount(&flash, "cache_read"), 0.003);
        assert_price(amount(&flash, "cache_write"), 0.003);
        assert_price(amount(&flash, "input"), 0.15);
        assert_price(amount(&flash, "output"), 0.60);
    }

    /// Peak doubles the same rate, and the list quotes whichever is in force.
    #[test]
    fn a_peak_window_is_quoted_at_double() {
        let off = amount(&entry(&list(OFFPEAK_MONDAY_12Z), "deepseek-flash"), "input");
        let on = amount(&entry(&list(PEAK_MONDAY_02Z), "deepseek-flash"), "input");
        assert_price(off, 0.15);
        assert_price(on, 0.30);
    }

    /// A withdrawn model is served by another model at that model's price, so a
    /// rate printed beside its own name would describe a bill nobody receives.
    #[test]
    fn a_withdrawn_model_is_not_listed() {
        let list = list(OFFPEAK_MONDAY_12Z);
        let listed: Vec<&str> = list["data"]
            .as_array()
            .expect("data is an array")
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert!(!listed.contains(&"deepseek-v4-pro"), "{listed:?}");
    }

    /// The envelope, the units and the picker's fields are what the two clients
    /// key on — a list missing either half breaks one of them silently.
    #[test]
    fn the_shape_serves_both_clients() {
        let list = list(OFFPEAK_MONDAY_12Z);
        assert_eq!(list["object"], "list");
        assert_eq!(list["as_of"], OFFPEAK_MONDAY_12Z);
        for m in list["data"].as_array().expect("data is an array") {
            assert_eq!(m["type"], "model");
            assert!(m["id"].is_string(), "{m}");
            // Both spellings of the name, and they agree: the picker reads one,
            // the price reader the other.
            assert_eq!(m["name"], m["id"], "{m}");
            assert_eq!(m["display_name"], m["id"], "{m}");
            for a in m["amounts"].as_array().expect("amounts is an array") {
                assert_eq!(a["currency"], "USD");
                assert_eq!(a["unit_tokens"], 1_000_000);
                assert!(a["price"].is_f64(), "{a}");
            }
        }
    }

    /// Two enabled hops can carry the same id, and one id must not appear twice
    /// with two different figures.
    #[test]
    fn a_model_id_is_listed_once() {
        let mut both = deepseek();
        both.providers
            .push(Preset::Deepseek.provider(Path::new("/tmp/tab-atelier-test")));
        let list = PriceListResource::of(&both, OFFPEAK_MONDAY_12Z);
        let ids: Vec<&str> = list.data.iter().map(|m| m.id.as_str()).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "{ids:?}");
    }
}
