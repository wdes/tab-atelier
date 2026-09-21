// SPDX-License-Identifier: MPL-2.0

//! What a session has cost: token counts, which sum, and amounts, which do not.
//!
//! The two are different kinds of number and are kept differently:
//!
//! * **Tokens are counts.** They add up regardless of who served the turn, so a session's
//!   total is one number per kind.
//! * **Amounts are money in a currency.** Two currencies cannot be added — a session that
//!   used one provider billed in USD and another in EUR has two totals, not one — so the
//!   amounts are an array, grouped by currency, and each group carries its own currency.
//! * **The model is a single value**: the last one used. "What am I running" has one answer
//!   even though the session may have spanned several.
//!
//! Prices come from the relay, which is the only party that knows what it charges: a
//! `GET /v1/models` on the same origin the session already reaches, so it works from a tab
//! with no internet. The response is a list of amounts *per model*, and each amount names its
//! own currency and unit — so one model may quote in two currencies, and the grouping is what
//! makes that legible rather than surprising.
//!
//! A model the catalog does not describe is not an error and not a guess: its tokens are
//! counted and no amount is invented for it. That is the difference between an unknown price
//! and a price of zero, and reporting the second when the first is true is how a cost display
//! becomes worse than none.

use std::collections::BTreeMap;

/// Which tokens an amount is charged for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Tokens sent that were not served from cache.
    Input,
    /// Tokens the model produced.
    Output,
    /// Tokens read out of the prompt cache — normally far cheaper than input.
    CacheRead,
    /// Tokens written into the prompt cache.
    CacheWrite,
}

impl Kind {
    /// Read one of these from a string, or nothing if it is not a kind.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "input" => Some(Self::Input),
            "output" => Some(Self::Output),
            // The wire spells these `cache_read_input_tokens` and friends, so the short forms,
            // the long ones, and the hyphenated variants all resolve: a price feed written by
            // hand should not fail over a separator.
            "cache_read" | "cacheread" | "cache-read" | "cache_read_input_tokens" => Some(Self::CacheRead),
            "cache_write" | "cachewrite" | "cache-write" | "cache_creation_input_tokens" => Some(Self::CacheWrite),
            _ => None,
        }
    }
}

/// Token counts for one turn, or for a session.
///
/// Four kinds rather than two, because the price of each differs — a cached read is roughly a
/// tenth of an input token — and a cost computed from input plus output alone is wrong on every
/// turn that used the cache, which is most of them here: the client sends three breakpoints on
/// every request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    /// This many tokens of one kind.
    #[must_use]
    pub const fn get(self, kind: Kind) -> u64 {
        match kind {
            Kind::Input => self.input,
            Kind::Output => self.output,
            Kind::CacheRead => self.cache_read,
            Kind::CacheWrite => self.cache_write,
        }
    }

    /// Add another turn's counts. Counts are counts, whoever served them.
    pub const fn add(&mut self, other: Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
    }

    /// Remove another turn's counts, saturating at zero.
    ///
    /// Used when a turn recorded as unpriced is priced later: the tokens move out of the unpriced
    /// tally as they move into an amount, so the same token is never both charged and reported as
    /// uncosted.
    pub const fn subtract(&mut self, other: Self) {
        self.input = self.input.saturating_sub(other.input);
        self.output = self.output.saturating_sub(other.output);
        self.cache_read = self.cache_read.saturating_sub(other.cache_read);
        self.cache_write = self.cache_write.saturating_sub(other.cache_write);
    }

    /// Whether nothing has been counted.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }

    /// Every token of every kind.
    ///
    /// The sum is what a *rate* applies to as a whole when no per-kind rate is known, which is the
    /// case that produced a confusing number: this figure and the `in - out` on the totals line
    /// counted different things, and neither said which. The difference is the cache, which on a
    /// session with prompt caching is most of the tokens — so a reader comparing the two had no way
    /// to reconcile them.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    /// The cache kinds together, for saying what part of [`Self::total`] the totals line leaves out.
    #[must_use]
    pub const fn cached(self) -> u64 {
        self.cache_read + self.cache_write
    }

    /// The counts, as JSON.
    #[must_use]
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "in": self.input,
            "out": self.output,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
        })
    }
}

/// One price line: an amount, in a currency, per a number of tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct Amount {
    pub currency: String,
    /// How many tokens the price covers. `1_000_000` is the usual, and stating it rather than
    /// implying it is what makes the arithmetic unambiguous for whoever writes the feed.
    pub unit_tokens: u64,
    pub kind: Kind,
    pub price: f64,
}

/// What the relay charges for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPrice {
    pub id: String,
    pub name: String,
    /// One entry per (currency, kind). A model may quote in more than one currency, which is
    /// why this is a list and not a single figure.
    pub amounts: Vec<Amount>,
}

impl ModelPrice {
    /// What a turn of these tokens costs, as `currency -> amount`.
    ///
    /// Only kinds the catalog prices contribute: a feed that omits cache prices produces a
    /// total that ignores cached tokens rather than one that charges them at the input rate,
    /// which would overstate a cached turn by roughly ten times.
    #[must_use]
    pub fn cost(&self, tokens: Tokens) -> BTreeMap<String, f64> {
        let mut out: BTreeMap<String, f64> = BTreeMap::new();
        for amount in &self.amounts {
            if amount.unit_tokens == 0 {
                // A feed that said "per zero tokens" would divide by zero. Skipping it is the
                // only reading that is not a silent infinity.
                continue;
            }
            let count = tokens.get(amount.kind);
            if count == 0 {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            // Token counts stay far inside f64's exact range (2^53), so this is lossless for
            // any real turn.
            let tokens_f = count as f64;
            #[allow(clippy::cast_precision_loss)]
            let unit_f = amount.unit_tokens as f64;
            *out.entry(amount.currency.clone()).or_insert(0.0) += tokens_f / unit_f * amount.price;
        }
        out
    }
}

/// The prices a relay serves, keyed by the model id it reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub models: Vec<ModelPrice>,
}

impl Catalog {
    /// Read a catalog from the relay's response.
    ///
    /// Lenient about the model list's own shape, strict about the amounts: an entry with no
    /// usable price is dropped rather than defaulted to zero, because a price of zero and an
    /// unstated price mean different things and only one of them is safe to display.
    pub fn parse(body: &str) -> Result<Self, String> {
        let parsed: serde_json::Value =
            serde_json::from_str(body).map_err(|e| format!("the price list is not valid JSON: {e}"))?;

        // Accept a bare list, a `{models: [...]}`, or an OpenAI-shaped `{data: [...]}`, since a
        // relay written to imitate a provider would naturally use the last of those.
        let list = parsed
            .get("models")
            .or_else(|| parsed.get("data"))
            .and_then(|v| v.as_array())
            .or_else(|| parsed.as_array())
            .ok_or_else(|| "the price list has no `models` array".to_string())?;

        let mut models = Vec::with_capacity(list.len());
        for entry in list {
            let Some(id) = entry.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(amounts) = entry.get("amounts").and_then(|v| v.as_array()) else {
                continue;
            };
            let parsed_amounts: Vec<Amount> = amounts.iter().filter_map(parse_amount).collect();
            if parsed_amounts.is_empty() {
                continue;
            }
            models.push(ModelPrice {
                id: id.to_owned(),
                name: entry.get("name").and_then(|v| v.as_str()).unwrap_or(id).to_owned(),
                amounts: parsed_amounts,
            });
        }
        Ok(Self { models })
    }

    /// The price for a model id the relay reported serving.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&ModelPrice> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Whether nothing usable was parsed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}

/// One amount from a catalog entry, or nothing if it is not usable.
fn parse_amount(raw: &serde_json::Value) -> Option<Amount> {
    let currency = raw
        .get("currency")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let kind = raw.get("kind").and_then(|v| v.as_str()).and_then(Kind::parse)?;
    let price = raw.get("price").and_then(serde_json::Value::as_f64)?;
    // A negative price is a feed error, not a discount this should apply silently.
    if price < 0.0 || !price.is_finite() {
        return None;
    }
    // Defaulted to a million because that is the convention for model pricing, and stated in
    // the feed whenever it differs.
    let unit_tokens = raw
        .get("unit_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1_000_000);
    Some(Amount {
        currency: currency.to_owned(),
        unit_tokens,
        kind,
        price,
    })
}

/// What a session has used, and what the relay says it costs.
///
/// Accumulated rather than recomputed: a session spans many turns and several providers, and
/// the prices arrive separately from the turns, so the running totals are the only place the
/// two meet.
#[derive(Debug, Default)]
pub struct Costs {
    catalog: Option<Catalog>,
    /// Currency -> total. A `BTreeMap` so the output order is stable: two runs with the same
    /// history must render identically, or nothing about it can be compared.
    amounts: BTreeMap<String, f64>,
    tokens: Tokens,
    /// The model last reported by the relay.
    model: Option<String>,
    /// Tokens from turns whose model the catalog does not price. Counted, never charged: an
    /// unknown price is not zero.
    unpriced_tokens: Tokens,
    /// Turns recorded before the catalog arrived, kept so they can be priced when it does.
    ///
    /// The price list is fetched in the background, deliberately: a session must not wait to be
    /// typeable on an enhancement. The cost of that is a window — usually milliseconds — in which
    /// a turn's model is not yet known, and without this the first turn of every session would be
    /// recorded as unpriced forever. Keeping the pairs closes the window instead of accepting a
    /// permanently wrong total.
    ///
    /// Bounded in practice by how long one fetch takes against how long a turn takes, and the
    /// number of turns before it lands is what it holds — normally nought or one.
    unbuffered: Vec<(String, Tokens)>,
}

impl Costs {
    /// Remember the prices, once they have been fetched.
    ///
    /// Turns recorded before this arrived are priced now: see [`Self::unbuffered`]. Anything still
    /// unpriced after the drain is a model the catalog genuinely does not describe, and stays
    /// reported as unpriced rather than being charged at zero.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        self.catalog = Some(catalog);
        let waiting = std::mem::take(&mut self.unbuffered);
        for (model, tokens) in waiting {
            // Out of the unpriced tally first, and *not* through `record`: the token counts were
            // already added when the turn happened, and counting them a second time here would
            // double the session's total. Only the money is settled now.
            self.unpriced_tokens.subtract(tokens);
            self.settle(&model, tokens);
        }
    }

    /// Whether a usable catalog has arrived.
    ///
    /// Distinguishes "no prices yet" from "prices saying nothing", which a fresh session must —
    /// an empty catalog would otherwise look like prices of zero.
    ///
    /// Gated to the tests because nothing in the program asks the question: the totals report an
    /// absent price by omitting an amount, which is the same answer without a method. Gating it
    /// keeps the absence of a caller visible rather than hidden behind an allow.
    #[cfg(test)]
    #[must_use]
    pub fn has_catalog(&self) -> bool {
        self.catalog.as_ref().is_some_and(|c| !c.is_empty())
    }

    /// The model the relay last reported serving.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Add one turn.
    ///
    /// The model is recorded whatever happens — it is what the relay *said*, and that is true
    /// whether or not a price is known for it.
    pub fn record(&mut self, model: &str, tokens: Tokens) {
        self.model = Some(model.to_owned());
        self.tokens.add(tokens);
        self.settle(model, tokens);
    }

    /// Charge a turn against the catalog, or hold it until one arrives.
    ///
    /// Split from [`Self::record`] because the two halves happen at different times when the price
    /// list is late: the counts are added when the turn happens, and the money is settled then or
    /// when the catalog lands. Doing both in one place meant the drain re-counted every buffered
    /// turn, doubling the session's tokens — which the test for it caught.
    ///
    /// Private, because calling it without counting would record a charge for tokens that were
    /// never tallied.
    fn settle(&mut self, model: &str, tokens: Tokens) {
        if let Some(price) = self.catalog.as_ref().and_then(|c| c.model(model)) {
            for (currency, amount) in price.cost(tokens) {
                *self.amounts.entry(currency).or_insert(0.0) += amount;
            }
        } else {
            // No price yet, or none coming. Reported as uncosted either way, and the pair is kept
            // so a catalog arriving later can price it — see `unbuffered`.
            self.unpriced_tokens.add(tokens);
            self.unbuffered.push((model.to_owned(), tokens));
        }
    }

    /// The running totals, in the shape the operator asked for: a summed token count, an array
    /// of amounts grouped by currency, and the model last used.
    #[must_use]
    pub fn totals_json(&self) -> serde_json::Value {
        let amounts: Vec<serde_json::Value> = self
            .amounts
            .iter()
            .map(|(currency, amount)| {
                serde_json::json!({
                    "currency": currency,
                    // More decimals than a cent: a single turn is often fractions of one, and
                    // rounding each currency's total to two places would show a session as free.
                    "amount": round_to(*amount, 6),
                })
            })
            .collect();

        let mut out = serde_json::json!({
            "model": self.model.clone().unwrap_or_default(),
            "tokens": self.tokens.to_json(),
            "amounts": amounts,
        });
        if !self.unpriced_tokens.is_empty() {
            // Said explicitly, so a smaller-than-expected total has an explanation on screen.
            out["unpriced_tokens"] = self.unpriced_tokens.to_json();
        }
        out
    }

    /// The tokens counted so far.
    #[must_use]
    pub const fn tokens(&self) -> Tokens {
        self.tokens
    }

    /// The money, one entry per currency, in a stable order.
    ///
    /// Values rather than the rendered JSON, so a caller that wants to phrase the total its own way
    /// does not have to take it apart again. The currency code travels with every amount because a
    /// bare number invites being read as some currency that has not been mentioned.
    #[must_use]
    pub fn amounts(&self) -> Vec<(String, f64)> {
        self.amounts.iter().map(|(c, a)| (c.clone(), *a)).collect()
    }

    /// Tokens from turns no price covered.
    ///
    /// Reported alongside the money so a total that looks low has an explanation on screen rather
    /// than inviting the reader to assume the prices are wrong.
    #[must_use]
    pub const fn unpriced(&self) -> Tokens {
        self.unpriced_tokens
    }
}

impl Costs {
    /// Restore the totals a session had already accumulated.
    ///
    /// The money comes back as it was *recorded*, not recomputed: the price list arrives later
    /// — and may have changed since — so recomputing would silently restate a session's history
    /// at today's prices. What was spent is a fact about the past.
    ///
    /// The catalog is deliberately left unset. It is fetched fresh, and until it arrives the
    /// restored amounts stand on their own.
    #[must_use]
    pub fn restore(spent: Option<&serde_json::Value>) -> Self {
        let mut costs = Self::default();
        let Some(spent) = spent else {
            return costs;
        };
        // The counts, from whichever shape the sidecar has: the grouped `cost.tokens` this build
        // writes, or the flat `input`/`output` at the top level that older ones carried. Read
        // *before* the `cost` block rather than inside it — a sidecar with no `cost` key at all is
        // exactly the old shape, and looking for the fallback only where a `cost` key exists meant
        // those sessions resumed having spent nothing.
        costs.tokens = spent.get("cost").and_then(|c| c.get("tokens")).map_or_else(
            || Tokens {
                input: spent.get("input").and_then(serde_json::Value::as_u64).unwrap_or(0),
                output: spent.get("output").and_then(serde_json::Value::as_u64).unwrap_or(0),
                ..Tokens::default()
            },
            read_tokens,
        );
        if let Some(cost) = spent.get("cost") {
            if let Some(model) = cost.get("model").and_then(|v| v.as_str()).filter(|m| !m.is_empty()) {
                costs.model = Some(model.to_owned());
            }
            if let Some(unpriced) = cost.get("unpriced_tokens") {
                costs.unpriced_tokens = read_tokens(unpriced);
            }
            if let Some(amounts) = cost.get("amounts").and_then(|v| v.as_array()) {
                for entry in amounts {
                    let (Some(currency), Some(amount)) = (
                        entry.get("currency").and_then(|v| v.as_str()),
                        entry.get("amount").and_then(serde_json::Value::as_f64),
                    ) else {
                        continue;
                    };
                    *costs.amounts.entry(currency.to_owned()).or_insert(0.0) += amount;
                }
            }
        }
        costs
    }
}

/// Token counts from a JSON object, tolerating a missing field.
fn read_tokens(value: &serde_json::Value) -> Tokens {
    let get = |name: &str| value.get(name).and_then(serde_json::Value::as_u64).unwrap_or(0);
    Tokens {
        input: get("in"),
        output: get("out"),
        cache_read: get("cache_read"),
        cache_write: get("cache_write"),
    }
}

/// The totals from a shared handle, for a caller that is not holding the lock.
///
/// A poisoned mutex yields a default rather than panicking: a cost display is not worth ending a
/// session over, and the numbers are the least important thing here.
#[must_use]
pub fn totals_of(costs: &std::sync::Arc<std::sync::Mutex<Costs>>) -> serde_json::Value {
    costs.lock().map_or_else(|_| serde_json::json!({}), |c| c.totals_json())
}

/// Round for display, keeping every currency at the same precision.
fn round_to(value: f64, places: u32) -> f64 {
    let factor = 10f64.powi(i32::try_from(places).unwrap_or(6));
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog with two providers and two currencies, which is the case that forces the
    /// grouping: the same session using both cannot report one total.
    fn two_currency_catalog() -> Catalog {
        Catalog::parse(
            r#"{
                "models": [
                    {
                        "id": "usd-model", "name": "One (USD)",
                        "amounts": [
                            { "currency": "USD", "unit_tokens": 1000000, "kind": "input", "price": 3.0 },
                            { "currency": "USD", "unit_tokens": 1000000, "kind": "output", "price": 15.0 },
                            { "currency": "USD", "unit_tokens": 1000000, "kind": "cache_read", "price": 0.3 }
                        ]
                    },
                    {
                        "id": "eur-model", "name": "Two (EUR)",
                        "amounts": [
                            { "currency": "EUR", "unit_tokens": 1000000, "kind": "input", "price": 2.0 },
                            { "currency": "EUR", "unit_tokens": 1000000, "kind": "output", "price": 10.0 }
                        ]
                    }
                ]
            }"#,
        )
        .expect("parses")
    }

    /// The arithmetic: tokens divided by the unit, times the price, per kind.
    #[test]
    fn a_models_cost_is_computed_per_kind_and_unit() {
        let catalog = two_currency_catalog();
        let model = catalog.model("usd-model").expect("priced");

        let cost = model.cost(Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 0,
        });
        // One unit of each: the prices are the amounts.
        assert_eq!(cost.get("USD"), Some(&18.3), "3 + 15 + 0.3");

        // A tenth of a unit is a tenth of the cost, so the unit really is applied.
        let cost = model.cost(Tokens {
            input: 100_000,
            ..Tokens::default()
        });
        assert!(
            (cost.get("USD").copied().unwrap_or_default() - 0.3).abs() < 1e-9,
            "{cost:?}"
        );
    }

    /// A kind the catalog does not price contributes nothing — it is not charged at the input
    /// rate, which would overstate a cached turn by roughly ten times.
    #[test]
    fn an_unpriced_kind_contributes_nothing() {
        let catalog = two_currency_catalog();
        let eur = catalog.model("eur-model").expect("priced");
        // This model lists no cache prices, and the turn is all cache reads.
        let cost = eur.cost(Tokens {
            cache_read: 5_000_000,
            ..Tokens::default()
        });
        assert!(cost.is_empty(), "nothing priced, so nothing charged: {cost:?}");
    }

    /// Two providers, two currencies: two totals, and neither absorbed the other.
    #[test]
    fn amounts_are_grouped_by_currency_and_never_summed_across_them() {
        let mut costs = Costs::default();
        costs.set_catalog(two_currency_catalog());

        costs.record(
            "usd-model",
            Tokens {
                input: 1_000_000,
                output: 1_000_000,
                ..Tokens::default()
            },
        );
        costs.record(
            "eur-model",
            Tokens {
                input: 1_000_000,
                output: 1_000_000,
                ..Tokens::default()
            },
        );

        let totals = costs.totals_json();
        let amounts = totals["amounts"].as_array().expect("an array");
        assert_eq!(amounts.len(), 2, "one entry per currency: {amounts:?}");
        assert_eq!(amounts[0]["currency"], "EUR");
        assert_eq!(amounts[0]["amount"], 12.0, "2 + 10");
        assert_eq!(amounts[1]["currency"], "USD");
        assert_eq!(amounts[1]["amount"], 18.0, "3 + 15");

        // Tokens, unlike money, sum: two turns of a million in each.
        assert_eq!(totals["tokens"]["in"], 2_000_000);
        assert_eq!(totals["tokens"]["out"], 2_000_000);

        // And the model is the *last* used, not the first and not a list.
        assert_eq!(totals["model"], "eur-model");
    }

    /// The same currency from two models adds up within its group.
    #[test]
    fn the_same_currency_from_two_models_accumulates_in_one_group() {
        let mut costs = Costs::default();
        costs.set_catalog(
            Catalog::parse(
                r#"{"models":[
                    {"id":"a","amounts":[{"currency":"USD","kind":"input","price":1.0}]},
                    {"id":"b","amounts":[{"currency":"USD","kind":"input","price":2.0}]}
                ]}"#,
            )
            .expect("parses"),
        );
        let unit = Tokens {
            input: 1_000_000,
            ..Tokens::default()
        };
        costs.record("a", unit);
        costs.record("b", unit);

        let totals = costs.totals_json();
        let amounts = totals["amounts"].as_array().expect("an array");
        assert_eq!(amounts.len(), 1, "one currency, so one group: {amounts:?}");
        assert_eq!(amounts[0]["amount"], 3.0, "1 + 2");
    }

    /// A model the catalog does not describe is reported as unpriced, not as free.
    #[test]
    fn an_unknown_model_is_counted_but_not_charged() {
        let mut costs = Costs::default();
        costs.set_catalog(two_currency_catalog());
        costs.record(
            "a-model-nobody-priced",
            Tokens {
                input: 1_000_000,
                output: 5,
                ..Tokens::default()
            },
        );

        let totals = costs.totals_json();
        assert!(
            totals["amounts"].as_array().expect("an array").is_empty(),
            "no amount may be invented: {totals}"
        );
        // The tokens are still counted, and the omission is stated.
        assert_eq!(totals["tokens"]["in"], 1_000_000);
        assert_eq!(totals["unpriced_tokens"]["in"], 1_000_000);
        // The model is still the last used, because that is what the relay said regardless.
        assert_eq!(totals["model"], "a-model-nobody-priced");
    }

    /// Before any catalog arrives, tokens are counted and nothing is charged.
    #[test]
    fn without_a_catalog_tokens_accumulate_and_no_amount_appears() {
        let mut costs = Costs::default();
        costs.record(
            "whatever",
            Tokens {
                input: 10,
                output: 20,
                ..Tokens::default()
            },
        );
        let totals = costs.totals_json();
        assert_eq!(totals["tokens"]["in"], 10);
        assert_eq!(totals["tokens"]["out"], 20);
        assert!(totals["amounts"].as_array().expect("an array").is_empty());
        assert!(!costs.has_catalog());
    }

    /// The catalog is read leniently, and an entry with no usable price is dropped rather than
    /// defaulted to zero.
    #[test]
    fn the_catalog_is_parsed_leniently_but_never_invents_a_price() {
        // The OpenAI-shaped list, and a bare list, both work.
        let openai =
            Catalog::parse(r#"{"data":[{"id":"m","amounts":[{"currency":"USD","kind":"output","price":1.0}]}]}"#)
                .expect("parses");
        assert_eq!(openai.models.len(), 1);

        let bare = Catalog::parse(r#"[{"id":"m","amounts":[{"currency":"USD","kind":"input","price":2.0}]}]"#)
            .expect("parses");
        let price = bare.model("m").expect("priced").amounts[0].price;
        assert!((price - 2.0).abs() < f64::EPSILON, "got {price}");

        // Entries that cannot be priced are dropped: no id, no amounts, a bad kind, a negative
        // price, a missing currency.
        let thin = Catalog::parse(
            r#"{"models":[
                {"amounts":[{"currency":"USD","kind":"input","price":1.0}]},
                {"id":"no-amounts"},
                {"id":"bad-kind","amounts":[{"currency":"USD","kind":"tokens","price":1.0}]},
                {"id":"negative","amounts":[{"currency":"USD","kind":"input","price":-1.0}]},
                {"id":"no-currency","amounts":[{"kind":"input","price":1.0}]},
                {"id":"empty","amounts":[]},
                {"id":"good","amounts":[{"currency":"USD","kind":"input","price":1.0}]}
            ]}"#,
        )
        .expect("parses");
        let ids: Vec<&str> = thin.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["good"], "only the usable entry survives: {ids:?}");

        // A body that is not a catalog at all is an error, so a caller can tell "no prices"
        // from "prices I could not read".
        assert!(Catalog::parse("<html>404</html>").is_err());
        assert!(Catalog::parse(r#"{"error":"nope"}"#).is_err());
    }

    /// A unit of zero would divide by zero; the amount is skipped instead.
    #[test]
    fn a_zero_unit_is_skipped_rather_than_dividing_by_zero() {
        let catalog = Catalog::parse(
            r#"{"models":[{"id":"m","amounts":[
                {"currency":"USD","unit_tokens":0,"kind":"input","price":5.0},
                {"currency":"USD","unit_tokens":1000,"kind":"output","price":1.0}
            ]}]}"#,
        )
        .expect("parses");
        let cost = catalog.model("m").expect("priced").cost(Tokens {
            input: 1000,
            output: 1000,
            ..Tokens::default()
        });
        assert_eq!(
            cost.get("USD"),
            Some(&1.0),
            "the zero-unit line contributes nothing: {cost:?}"
        );
    }

    /// Summing many small turns must not drift into nonsense: a session's total is a sum of
    /// fractions of a cent, which is where f64 could bite.
    #[test]
    fn many_small_turns_sum_without_drifting() {
        let mut costs = Costs::default();
        costs.set_catalog(
            Catalog::parse(
                r#"{"models":[{"id":"m","amounts":[
                    {"currency":"USD","unit_tokens":1000000,"kind":"input","price":0.28}
                ]}]}"#,
            )
            .expect("parses"),
        );
        // Ten thousand turns of a thousand input tokens: 0.00028 each, so 2.8 in total.
        for _ in 0..10_000 {
            costs.record(
                "m",
                Tokens {
                    input: 1_000,
                    ..Tokens::default()
                },
            );
        }
        let totals = costs.totals_json();
        let amount = totals["amounts"][0]["amount"].as_f64().expect("a number");
        assert!(
            (amount - 2.8).abs() < 1e-6,
            "ten thousand small turns should total 2.8, got {amount}"
        );
        assert_eq!(totals["tokens"]["in"], 10_000_000);
    }

    /// The kind names accept the spellings a hand-written feed would use.
    #[test]
    fn price_kinds_accept_their_wire_spellings() {
        for name in ["input", " Input "] {
            assert_eq!(Kind::parse(name), Some(Kind::Input));
        }
        for name in ["cache_read", "cache-read", "cache_read_input_tokens", "CACHE_READ"] {
            assert_eq!(Kind::parse(name), Some(Kind::CacheRead), "{name}");
        }
        for name in ["cache_write", "cache_creation_input_tokens"] {
            assert_eq!(Kind::parse(name), Some(Kind::CacheWrite), "{name}");
        }
        assert_eq!(Kind::parse("tokens"), None);
        assert_eq!(Kind::parse(""), None);
    }
    /// A turn recorded before the price list arrives is priced when it does.
    ///
    /// The fetch is in the background on purpose — a session must not wait to be typeable on an
    /// enhancement — so there is a window, usually milliseconds, in which a turn's model is not yet
    /// known. Without buffering, the first turn of every session would be recorded as unpriced
    /// forever, and "unpriced" would stop meaning "this relay does not describe the model".
    #[test]
    fn a_turn_recorded_before_the_prices_arrive_is_priced_when_they_do() {
        let mut costs = Costs::default();
        let turn = Tokens {
            input: 1_000_000,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        };

        // The turn lands first, with no catalog at all.
        costs.record("usd-model", turn);
        let before = costs.totals_json();
        assert!(
            before["amounts"].as_array().expect("an array").is_empty(),
            "nothing can be charged yet: {before}"
        );
        assert_eq!(before["unpriced_tokens"]["in"], 1_000_000, "but it is counted");

        // Then the prices arrive.
        costs.set_catalog(two_currency_catalog());

        let after = costs.totals_json();
        assert_eq!(
            after["amounts"][0]["amount"], 3.0,
            "the earlier turn is now charged at its own model's rate: {after}"
        );
        assert_eq!(after["amounts"][0]["currency"], "USD");
        assert!(
            after.get("unpriced_tokens").is_none(),
            "and it is no longer reported as uncosted — charged *and* uncosted would be wrong \
             twice over: {after}"
        );
        // The tokens were counted once throughout.
        assert_eq!(after["tokens"]["in"], 1_000_000);
    }

    /// A model the catalog genuinely does not describe stays unpriced after the drain.
    #[test]
    fn a_model_the_catalog_lacks_stays_unpriced() {
        let mut costs = Costs::default();
        costs.record(
            "usd-model",
            Tokens {
                input: 1_000_000,
                ..Tokens::default()
            },
        );
        costs.record(
            "nobody-prices-this",
            Tokens {
                input: 2_000_000,
                ..Tokens::default()
            },
        );
        costs.set_catalog(two_currency_catalog());

        let totals = costs.totals_json();
        assert_eq!(totals["amounts"][0]["amount"], 3.0, "only the priced one: {totals}");
        assert_eq!(
            totals["unpriced_tokens"]["in"], 2_000_000,
            "and the other is still declared uncosted: {totals}"
        );
    }

    /// A restored session keeps its money and its counts.
    ///
    /// The money comes back as it was recorded rather than recomputed: the price list arrives after
    /// the restore, and may have changed since, so recomputing would restate a session's history at
    /// today's prices. What was spent is a fact about the past.
    #[test]
    fn a_restored_session_carries_on_from_what_it_had_spent() {
        let stored = serde_json::json!({
            "input": 120,
            "output": 80,
            "cost": {
                "model": "eur-model",
                "tokens": { "in": 120, "out": 80, "cache_read": 0, "cache_write": 0 },
                "amounts": [ { "currency": "EUR", "amount": 1.25 } ]
            }
        });

        let mut costs = Costs::restore(Some(&stored));
        // What it had, then one more turn on top.
        let before = costs.totals_json();
        assert_eq!(before["model"], "eur-model", "the model it last used");
        assert_eq!(before["amounts"][0]["currency"], "EUR");
        assert_eq!(before["amounts"][0]["amount"], 1.25);
        assert_eq!(before["tokens"]["in"], 120);

        costs.set_catalog(two_currency_catalog());
        costs.record(
            "usd-model",
            Tokens {
                input: 1_000_000,
                ..Tokens::default()
            },
        );

        let after = costs.totals_json();
        let amounts = after["amounts"].as_array().expect("an array");
        assert_eq!(amounts.len(), 2, "the restored euro and the new dollar: {amounts:?}");
        assert_eq!(amounts[0]["currency"], "EUR");
        assert_eq!(amounts[0]["amount"], 1.25, "the restored figure is not restated");
        assert_eq!(amounts[1]["currency"], "USD");
        assert_eq!(amounts[1]["amount"], 3.0);
        // Counts accumulated across the resume.
        assert_eq!(after["tokens"]["in"], 1_000_120);
        // And the model is the one just used, not the restored one.
        assert_eq!(after["model"], "usd-model");
    }

    /// A sidecar from before the grouped shape existed still restores its counts.
    #[test]
    fn the_older_flat_sidecar_shape_still_restores() {
        let stored = serde_json::json!({ "input": 42, "output": 7 });
        let costs = Costs::restore(Some(&stored));
        let totals = costs.totals_json();
        assert_eq!(totals["tokens"]["in"], 42);
        assert_eq!(totals["tokens"]["out"], 7);
        // No cost block, so nothing is claimed about money.
        assert!(totals["amounts"].as_array().expect("an array").is_empty());
    }

    /// Restoring nothing is a fresh session, and a malformed sidecar is not fatal.
    #[test]
    fn restoring_nothing_or_junk_gives_a_clean_session() {
        for spent in [
            None,
            Some(&serde_json::json!(null)),
            Some(&serde_json::json!("nonsense")),
        ] {
            let costs = Costs::restore(spent);
            let totals = costs.totals_json();
            assert_eq!(totals["tokens"]["in"], 0);
            assert_eq!(totals["model"], "");
            assert!(totals["amounts"].as_array().expect("an array").is_empty());
        }
    }
}
