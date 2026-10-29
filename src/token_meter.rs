//! FOCUS 1.5 token billing: what a token-metered row says about its tokens.
//!
//! FOCUS 1.5 describes an AI model API's bill without adding a column for
//! it. The raw count goes in `ConsumedQuantity` with `ConsumedUnit`
//! `"Tokens"`, the provider's meter name in `SkuMeter`, and four kinds of
//! fact go in the `SkuPriceDetails` JSON object:
//!
//! - **model identity**: `ModelDeveloper`, `ModelId` (and `ModelFamily`,
//!   `ModelVersion`, which no source here reports);
//! - **`TokenDirection`**: `"Input"` for tokens consumed from a request,
//!   `"Output"` for tokens generated in a response, absent when a meter
//!   counts both;
//! - **`TokenCacheAction`**: on input tokens only, `"Read"` for tokens
//!   served from a prompt cache, `"Write"` for tokens placed into one,
//!   `"Uncached"` for input that did neither, absent when a meter mixes
//!   them.
//!
//! The model-provider imports name a row's tokens by its pricing unit —
//! `"Cache Read Tokens"`, `"Output Tokens"` — so [`fill`] derives the FOCUS
//! columns from it at write time, the way [`crate::service_category::fill`]
//! places a product. A source that reports FOCUS itself fills them first
//! and is left alone.
//!
//! Pure and target-independent: both backends call [`fill`] on the way in.

use serde_json::{Map, Value};

use crate::model::{Charge, TokenClass};

/// FOCUS `ConsumedUnit` for a token count.
pub const TOKENS: &str = "Tokens";

/// FOCUS `TokenDirection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenDirection {
    Input,
    Output,
}

impl TokenDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "Input",
            Self::Output => "Output",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Input" => Some(Self::Input),
            "Output" => Some(Self::Output),
            _ => None,
        }
    }
}

/// FOCUS `TokenCacheAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenCacheAction {
    Uncached,
    Read,
    Write,
    Other,
}

impl TokenCacheAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uncached => "Uncached",
            Self::Read => "Read",
            Self::Write => "Write",
            Self::Other => "Other",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Uncached" => Some(Self::Uncached),
            "Read" => Some(Self::Read),
            "Write" => Some(Self::Write),
            "Other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// The token properties of one meter. Either may be absent: FOCUS leaves
/// out a property the meter cannot answer rather than guessing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TokenMeter {
    pub direction: Option<TokenDirection>,
    pub cache_action: Option<TokenCacheAction>,
}

/// What a pricing unit the ledger stores says about its tokens, or `None`
/// for a unit that does not count tokens. Case-insensitive and trimmed, so
/// a provider's own spelling still matches.
///
/// - `"Input Tokens"` is input that touched no cache. The importers make it
///   so: Anthropic reports cache reads and writes apart from input, and
///   OpenAI's input count has its cached tokens taken out on import.
/// - `"Tokens"`, the aggregate Alibaba Cloud's Model Studio and Volcengine's
///   Ark bill in, counts input and output together, so it has neither
///   property.
pub fn meter_of_unit(unit: &str) -> Option<TokenMeter> {
    use TokenCacheAction::{Read, Uncached, Write};
    use TokenDirection::{Input, Output};
    let meter = |direction, cache_action| {
        Some(TokenMeter {
            direction,
            cache_action,
        })
    };
    match unit.trim().to_ascii_lowercase().as_str() {
        "input tokens" => meter(Some(Input), Some(Uncached)),
        "output tokens" => meter(Some(Output), None),
        "cached input tokens" | "cache read tokens" => meter(Some(Input), Some(Read)),
        "cache creation tokens" => meter(Some(Input), Some(Write)),
        "tokens" => meter(None, None),
        _ => None,
    }
}

/// FOCUS `ModelDeveloper` for a model provider's own bill. A provider that
/// resells other developers' models — a cloud marketplace — reports the
/// developer itself, so only the first-party sources are listed.
pub fn model_developer(provider: &str) -> Option<&'static str> {
    match provider {
        "OpenAI" => Some("OpenAI"),
        "Anthropic" => Some("Anthropic"),
        "DeepSeek" => Some("DeepSeek"),
        _ => None,
    }
}

/// The FOCUS-defined `SkuPriceDetails` properties a token bill carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenDetails {
    pub model_developer: Option<String>,
    pub model_id: Option<String>,
    pub meter: TokenMeter,
}

impl TokenDetails {
    /// `SkuPriceDetails` JSON text, keys in alphabetical order as the
    /// specification's examples list them. `None` when there is nothing to
    /// say.
    pub fn to_json(&self) -> Option<String> {
        let mut object = Map::new();
        let mut put = |key: &str, value: Option<&str>| {
            if let Some(value) = value {
                object.insert(key.to_string(), Value::String(value.to_string()));
            }
        };
        put("ModelDeveloper", self.model_developer.as_deref());
        put("ModelId", self.model_id.as_deref());
        put(
            "TokenCacheAction",
            self.meter.cache_action.map(TokenCacheAction::as_str),
        );
        put(
            "TokenDirection",
            self.meter.direction.map(TokenDirection::as_str),
        );
        (!object.is_empty()).then(|| Value::Object(object).to_string())
    }

    /// Read the token properties back out of `SkuPriceDetails` text. Other
    /// properties are ignored, and so is a value FOCUS does not allow.
    pub fn parse(json: &str) -> Self {
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(json) else {
            return Self::default();
        };
        let text = |key: &str| object.get(key).and_then(Value::as_str);
        Self {
            model_developer: text("ModelDeveloper").map(str::to_string),
            model_id: text("ModelId").map(str::to_string),
            meter: TokenMeter {
                direction: text("TokenDirection").and_then(TokenDirection::parse),
                cache_action: text("TokenCacheAction").and_then(TokenCacheAction::parse),
            },
        }
    }
}

/// Which part of a token bill a row's tokens are, from its FOCUS columns:
/// `None` when `ConsumedUnit` is not `"Tokens"`. A row with no
/// `TokenDirection` meters input and output together. Input with no
/// `TokenCacheAction`, or `"Other"`, is input the bill does not place in a
/// cache, so it counts as plain input.
pub fn class_of(
    consumed_unit: Option<&str>,
    sku_price_details: Option<&str>,
) -> Option<TokenClass> {
    if !consumed_unit.is_some_and(|unit| unit.trim().eq_ignore_ascii_case(TOKENS)) {
        return None;
    }
    let meter = sku_price_details
        .map(|details| TokenDetails::parse(details).meter)
        .unwrap_or_default();
    Some(match (meter.direction, meter.cache_action) {
        (Some(TokenDirection::Output), _) => TokenClass::Output,
        (Some(TokenDirection::Input), Some(TokenCacheAction::Read)) => TokenClass::CacheRead,
        (Some(TokenDirection::Input), Some(TokenCacheAction::Write)) => TokenClass::CacheWrite,
        (Some(TokenDirection::Input), _) => TokenClass::Input,
        (None, _) => TokenClass::Unsplit,
    })
}

/// The FOCUS 1.5 columns a row gains from its pricing unit and model.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TokenColumns {
    pub consumed_quantity: Option<f64>,
    pub consumed_unit: Option<String>,
    pub sku_meter: Option<String>,
    pub sku_price_details: Option<String>,
}

/// [`fill`]'s answer for one row, from the columns the ledger had before
/// FOCUS 1.5: what a migration backfills and what a new write gains. `None`
/// for a row with neither a token unit nor a model.
pub fn derive(
    provider: &str,
    pricing_unit: Option<&str>,
    pricing_quantity: Option<f64>,
    x_model: Option<&str>,
) -> Option<TokenColumns> {
    let meter = pricing_unit.and_then(meter_of_unit);
    let model_id = x_model.map(str::trim).filter(|model| !model.is_empty());
    if meter.is_none() && model_id.is_none() {
        return None;
    }

    let details = TokenDetails {
        // A developer without a model would describe no SKU.
        model_developer: model_id.and(model_developer(provider)).map(str::to_string),
        model_id: model_id.map(str::to_string),
        meter: meter.unwrap_or_default(),
    };
    Some(TokenColumns {
        consumed_quantity: meter.and(pricing_quantity),
        consumed_unit: meter.map(|_| TOKENS.to_string()),
        // The pricing unit is the provider's name for the meter.
        sku_meter: meter.and(pricing_unit).map(|unit| unit.trim().to_string()),
        sku_price_details: details.to_json(),
    })
}

/// What a token meter's name says about its tokens, for a FOCUS row that
/// predates `TokenDirection` and `TokenCacheAction`: a FOCUS 1.2 export,
/// whose meter or description still names the kind of token, as in
/// `"Claude3.5Sonnet-cache-read-input-token-count"`. Matched on words, so
/// `-`, `_` and case do not matter. `None` when the name says neither
/// direction.
///
/// A cache miss is input served from no cache. Input the name says nothing
/// more about has no `TokenCacheAction`: FOCUS leaves out what the meter
/// does not tell.
pub fn meter_from_name(name: &str) -> Option<TokenMeter> {
    use TokenCacheAction::{Read, Uncached, Write};
    use TokenDirection::{Input, Output};
    let words: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { ' ' })
        .collect();
    let words = format!(
        " {} ",
        words.split_whitespace().collect::<Vec<_>>().join(" ")
    );
    let has = |phrase: &str| words.contains(&format!(" {phrase} "));
    let meter = |direction, cache_action| {
        Some(TokenMeter {
            direction: Some(direction),
            cache_action,
        })
    };

    if has("cache read") || has("cached input") || has("cache hit") || has("cached") {
        meter(Input, Some(Read))
    } else if has("cache write") || has("cache creation") || has("cache writes") {
        meter(Input, Some(Write))
    } else if has("cache miss") || has("uncached") {
        meter(Input, Some(Uncached))
    } else if has("output") || has("completion") || has("generated") || has("response") {
        meter(Output, None)
    } else if has("input") || has("prompt") {
        meter(Input, None)
    } else {
        None
    }
}

/// A FOCUS token row whose details give no `TokenDirection`, completed from
/// its meter name or description by [`meter_from_name`]. The properties it
/// does carry are kept; details that are not a JSON object are left alone.
fn complete_focus_row(charge: &mut Charge) {
    if class_of(
        charge.consumed_unit.as_deref(),
        charge.sku_price_details.as_deref(),
    ) != Some(TokenClass::Unsplit)
    {
        return;
    }
    let Some(meter) = [
        charge.sku_meter.as_deref(),
        charge.charge_description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(meter_from_name) else {
        return;
    };

    let mut object = match charge.sku_price_details.as_deref() {
        None => Map::new(),
        Some(json) => match serde_json::from_str::<Value>(json) {
            Ok(Value::Object(object)) => object,
            _ => return,
        },
    };
    if let Some(direction) = meter.direction {
        object.insert(
            "TokenDirection".to_string(),
            Value::String(direction.as_str().to_string()),
        );
    }
    if let Some(cache_action) = meter.cache_action {
        object
            .entry("TokenCacheAction")
            .or_insert_with(|| Value::String(cache_action.as_str().to_string()));
    }
    // Keys in alphabetical order, as [`TokenDetails::to_json`] writes them,
    // whichever order the export used.
    let sorted: std::collections::BTreeMap<String, Value> = object.into_iter().collect();
    charge.sku_price_details = serde_json::to_string(&sorted).ok();
}

/// Give a token-metered or model-attributed charge its FOCUS 1.5 columns.
/// A charge that already has any of them came from a source that reports
/// FOCUS itself, and keeps what it reported, completed by
/// [`meter_from_name`] when it is a token row of no stated direction.
pub fn fill(provider: &str, charge: &mut Charge) {
    if charge.consumed_unit.is_some() || charge.sku_price_details.is_some() {
        complete_focus_row(charge);
        return;
    }
    let Some(columns) = derive(
        provider,
        charge.pricing_unit.as_deref(),
        charge.pricing_quantity,
        charge.x_model.as_deref(),
    ) else {
        return;
    };
    charge.consumed_quantity = charge.consumed_quantity.or(columns.consumed_quantity);
    charge.consumed_unit = columns.consumed_unit;
    charge.sku_meter = charge.sku_meter.take().or(columns.sku_meter);
    charge.sku_price_details = columns.sku_price_details;
}

#[cfg(test)]
mod tests {
    use super::{
        derive, fill, meter_of_unit, TokenCacheAction, TokenDetails, TokenDirection, TokenMeter,
        TOKENS,
    };
    use crate::model::Charge;
    use chrono::Utc;

    fn meter(unit: &str) -> (Option<&'static str>, Option<&'static str>) {
        let meter = meter_of_unit(unit).expect("a token unit");
        (
            meter.direction.map(TokenDirection::as_str),
            meter.cache_action.map(TokenCacheAction::as_str),
        )
    }

    #[test]
    fn each_unit_the_importers_write_has_its_focus_properties() {
        assert_eq!(meter("Input Tokens"), (Some("Input"), Some("Uncached")));
        assert_eq!(meter("Output Tokens"), (Some("Output"), None));
        assert_eq!(meter("Cached Input Tokens"), (Some("Input"), Some("Read")));
        assert_eq!(meter("Cache Read Tokens"), (Some("Input"), Some("Read")));
        assert_eq!(
            meter("Cache Creation Tokens"),
            (Some("Input"), Some("Write"))
        );
        // An aggregate of input and output carries neither property.
        assert_eq!(meter("Tokens"), (None, None));
        assert_eq!(
            meter("  CACHE READ TOKENS \n"),
            (Some("Input"), Some("Read"))
        );
    }

    #[test]
    fn a_row_is_classed_by_its_focus_properties() {
        use super::class_of;
        use crate::model::TokenClass;
        let class = |unit: &str| {
            let columns = derive("Anthropic", Some(unit), Some(1.0), Some("m")).unwrap();
            class_of(
                columns.consumed_unit.as_deref(),
                columns.sku_price_details.as_deref(),
            )
        };
        assert_eq!(class("Input Tokens"), Some(TokenClass::Input));
        assert_eq!(class("Output Tokens"), Some(TokenClass::Output));
        assert_eq!(class("Cache Read Tokens"), Some(TokenClass::CacheRead));
        assert_eq!(class("Cache Creation Tokens"), Some(TokenClass::CacheWrite));
        assert_eq!(class("Tokens"), Some(TokenClass::Unsplit));

        // As a FOCUS export might write them: input on one meter whether
        // or not it touched a cache.
        assert_eq!(
            class_of(Some("Tokens"), Some(r#"{"TokenDirection":"Input"}"#)),
            Some(TokenClass::Input)
        );
        assert_eq!(
            class_of(
                Some("tokens"),
                Some(r#"{"TokenDirection":"Input","TokenCacheAction":"Other"}"#)
            ),
            Some(TokenClass::Input)
        );
        assert_eq!(class_of(Some("Tokens"), None), Some(TokenClass::Unsplit));
        assert_eq!(
            class_of(Some("Token-Hours"), Some(r#"{"ModelId":"m"}"#)),
            None
        );
        assert_eq!(class_of(None, Some(r#"{"TokenDirection":"Output"}"#)), None);
    }

    #[test]
    fn a_unit_that_counts_no_tokens_has_no_meter() {
        assert_eq!(meter_of_unit("Requests"), None);
        assert_eq!(meter_of_unit("Web Search Requests"), None);
        assert_eq!(meter_of_unit("GB-Mo"), None);
        assert_eq!(meter_of_unit(""), None);
    }

    #[test]
    fn details_are_written_as_the_specification_spells_them() {
        let details = TokenDetails {
            model_developer: Some("Anthropic".to_string()),
            model_id: Some("claude-sonnet-4-5".to_string()),
            meter: TokenMeter {
                direction: Some(TokenDirection::Input),
                cache_action: Some(TokenCacheAction::Read),
            },
        };
        let json = details.to_json().unwrap();
        assert_eq!(
            json,
            r#"{"ModelDeveloper":"Anthropic","ModelId":"claude-sonnet-4-5","TokenCacheAction":"Read","TokenDirection":"Input"}"#
        );
        assert_eq!(TokenDetails::parse(&json), details);
        assert_eq!(TokenDetails::default().to_json(), None);
    }

    #[test]
    fn parsing_ignores_what_focus_does_not_allow() {
        let details = TokenDetails::parse(
            r#"{"TokenDirection":"Sideways","TokenCacheAction":"Read","VCPU":4}"#,
        );
        assert_eq!(details.meter.direction, None);
        assert_eq!(details.meter.cache_action, Some(TokenCacheAction::Read));
        assert_eq!(TokenDetails::parse("not json"), TokenDetails::default());
    }

    #[test]
    fn a_token_row_gains_its_consumed_quantity_meter_and_details() {
        let columns = derive(
            "OpenAI",
            Some("Cached Input Tokens"),
            Some(4000.0),
            Some("gpt-5"),
        )
        .unwrap();
        assert_eq!(columns.consumed_quantity, Some(4000.0));
        assert_eq!(columns.consumed_unit.as_deref(), Some(TOKENS));
        assert_eq!(columns.sku_meter.as_deref(), Some("Cached Input Tokens"));
        assert_eq!(
            columns.sku_price_details.as_deref(),
            Some(
                r#"{"ModelDeveloper":"OpenAI","ModelId":"gpt-5","TokenCacheAction":"Read","TokenDirection":"Input"}"#
            )
        );
    }

    #[test]
    fn a_model_row_without_tokens_keeps_its_identity_only() {
        // A cost row covering two token types, or DeepSeek's cost export.
        let columns = derive("DeepSeek", None, None, Some("deepseek-chat")).unwrap();
        assert_eq!(columns.consumed_quantity, None);
        assert_eq!(columns.consumed_unit, None);
        assert_eq!(columns.sku_meter, None);
        assert_eq!(
            columns.sku_price_details.as_deref(),
            Some(r#"{"ModelDeveloper":"DeepSeek","ModelId":"deepseek-chat"}"#)
        );
    }

    #[test]
    fn a_resold_or_unnamed_model_claims_no_developer() {
        // Model Studio bills a product, not a model.
        let columns = derive("Aliyun", Some("Tokens"), Some(9.0), None).unwrap();
        assert_eq!(columns.consumed_unit.as_deref(), Some(TOKENS));
        assert_eq!(columns.sku_price_details, None);

        let columns = derive("Aliyun", Some("Tokens"), Some(9.0), Some("qwen-max")).unwrap();
        assert_eq!(
            columns.sku_price_details.as_deref(),
            Some(r#"{"ModelId":"qwen-max"}"#)
        );
    }

    #[test]
    fn a_row_with_neither_tokens_nor_a_model_is_left_alone() {
        assert_eq!(derive("AWS", Some("Hrs"), Some(24.0), None), None);
        assert_eq!(derive("AWS", None, None, Some("  ")), None);
    }

    #[test]
    fn a_meter_name_says_which_tokens_it_counts() {
        use super::meter_from_name;
        let named = |name: &str| {
            meter_from_name(name).map(|meter| {
                (
                    meter.direction.map(TokenDirection::as_str),
                    meter.cache_action.map(TokenCacheAction::as_str),
                )
            })
        };
        assert_eq!(
            named("USE1-Claude3.5Sonnet-cache-read-input-token-count"),
            Some((Some("Input"), Some("Read")))
        );
        assert_eq!(
            named("USW2-Claude3.7Sonnet-cache-write-input-token-count"),
            Some((Some("Input"), Some("Write")))
        );
        assert_eq!(
            named("input_cache_miss_tokens"),
            Some((Some("Input"), Some("Uncached")))
        );
        assert_eq!(
            named("Cached Input Tokens"),
            Some((Some("Input"), Some("Read")))
        );
        assert_eq!(
            named("USE1-Nova-Pro-output-tokens"),
            Some((Some("Output"), None))
        );
        assert_eq!(named("completion tokens"), Some((Some("Output"), None)));
        assert_eq!(
            named("USE1-Llama3-input-tokens"),
            Some((Some("Input"), None))
        );
        // Words, not substrings: "throughput" is not "output".
        assert_eq!(named("Provisioned throughput model units"), None);
        assert_eq!(named("Tokens"), None);
    }

    #[test]
    fn a_focus_token_row_without_a_direction_is_completed_from_its_meter() {
        let row = |details: Option<&str>, meter: &str| Charge {
            consumed_quantity: Some(1000.0),
            consumed_unit: Some(TOKENS.to_string()),
            sku_meter: Some(meter.to_string()),
            sku_price_details: details.map(str::to_string),
            ..Charge::new(Utc::now(), Utc::now(), "USD")
        };

        let mut charge = row(
            Some(r#"{"ModelId":"anthropic.claude-3-5"}"#),
            "Claude-cache-read-input-token-count",
        );
        fill("AWS", &mut charge);
        assert_eq!(
            charge.sku_price_details.as_deref(),
            Some(
                r#"{"ModelId":"anthropic.claude-3-5","TokenCacheAction":"Read","TokenDirection":"Input"}"#
            )
        );

        // No details at all: the meter is enough.
        let mut charge = row(None, "Nova-output-tokens");
        fill("AWS", &mut charge);
        assert_eq!(
            charge.sku_price_details.as_deref(),
            Some(r#"{"TokenDirection":"Output"}"#)
        );

        // A row that states its direction keeps it, whatever the meter says.
        let stated = r#"{"TokenDirection":"Input"}"#;
        let mut charge = row(Some(stated), "something-output-tokens");
        fill("AWS", &mut charge);
        assert_eq!(charge.sku_price_details.as_deref(), Some(stated));

        // Details that are not an object are not rewritten.
        let mut charge = row(Some("[1,2]"), "Nova-output-tokens");
        fill("AWS", &mut charge);
        assert_eq!(charge.sku_price_details.as_deref(), Some("[1,2]"));

        // Not a token row: nothing to complete.
        let mut charge = Charge {
            consumed_unit: Some("Hours".to_string()),
            ..row(None, "BoxUsage-output")
        };
        fill("AWS", &mut charge);
        assert_eq!(charge.sku_price_details, None);
    }

    #[test]
    fn fill_keeps_what_a_focus_source_reported() {
        let mut charge = Charge {
            pricing_unit: Some("Output Tokens".to_string()),
            pricing_quantity: Some(10.0),
            x_model: Some("gpt-5".to_string()),
            sku_price_details: Some(r#"{"ModelId":"vendor-name"}"#.to_string()),
            ..Charge::new(Utc::now(), Utc::now(), "USD")
        };
        fill("OpenAI", &mut charge);
        assert_eq!(charge.consumed_unit, None);
        assert_eq!(
            charge.sku_price_details.as_deref(),
            Some(r#"{"ModelId":"vendor-name"}"#)
        );

        let mut charge = Charge {
            sku_price_details: None,
            ..charge
        };
        fill("OpenAI", &mut charge);
        assert_eq!(charge.consumed_quantity, Some(10.0));
        assert_eq!(charge.sku_meter.as_deref(), Some("Output Tokens"));
        assert_eq!(
            charge.sku_price_details.as_deref(),
            Some(r#"{"ModelDeveloper":"OpenAI","ModelId":"gpt-5","TokenDirection":"Output"}"#)
        );
    }
}
