//! The structured reply envelope the AI engine returns, and its mapping to the
//! channel-agnostic `CanonicalReply`.
//!
//! `agent-runtime`'s `run_structured` forces the model's final answer to match
//! `AiReply`'s JSON Schema, so this is *intent* expressed as data — the engine
//! never decides presentation (buttons vs. list vs. text). Each channel adapter
//! renders the resulting `CanonicalReply` natively.
//!
//! Pure (serde + schemars only) so it compiles and unit-tests on native.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::protocol::{CanonicalReply, CartCard, CartItem, Choice};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AiChoiceOption {
    /// Stable id echoed back when the user picks this option.
    pub id: String,
    /// Human-readable label shown on the button/list row.
    pub label: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AiCartLine {
    pub name: String,
    pub qty: u32,
    #[serde(default)]
    pub price_cents: Option<u64>,
}

/// The model's final answer, constrained to this schema. `kind` selects which
/// optional fields are meaningful.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AiReply {
    /// One of `"text"`, `"choices"`, or `"cart"`.
    pub kind: String,
    /// The message to show the user. Used for `kind="text"`; an optional header
    /// for the others.
    #[serde(default)]
    pub text: Option<String>,
    /// For `kind="choices"`: the question to ask.
    #[serde(default)]
    pub prompt: Option<String>,
    /// For `kind="choices"`: the selectable options.
    #[serde(default)]
    pub options: Option<Vec<AiChoiceOption>>,
    /// For `kind="cart"`: the current cart lines.
    #[serde(default)]
    pub cart_items: Option<Vec<AiCartLine>>,
    /// For `kind="cart"`: total in cents.
    #[serde(default)]
    pub total_cents: Option<u64>,
    /// For `kind="cart"`: native-app checkout hand-off URL (we never custody
    /// the user's money — they confirm + pay in Sixty60).
    #[serde(default)]
    pub checkout_url: Option<String>,
    /// Control signal: the store session is gone and the user must re-auth.
    #[serde(default)]
    pub needs_reauth: Option<bool>,
}

impl AiReply {
    /// Map the model's intent onto the channel-agnostic reply the adapters render.
    pub fn to_canonical(&self) -> CanonicalReply {
        match self.kind.as_str() {
            "choices" => CanonicalReply::Choice {
                prompt: self
                    .prompt
                    .clone()
                    .or_else(|| self.text.clone())
                    .unwrap_or_else(|| "Please choose:".to_string()),
                options: self
                    .options
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|o| Choice {
                        id: o.id,
                        label: o.label,
                    })
                    .collect(),
            },
            "cart" => CanonicalReply::Cart(CartCard {
                items: self
                    .cart_items
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|i| CartItem {
                        name: i.name,
                        qty: i.qty,
                        price_cents: i.price_cents,
                    })
                    .collect(),
                total_cents: self.total_cents,
                checkout_url: self.checkout_url.clone(),
            }),
            // Default to a plain text reply for "text" or any unexpected kind.
            _ => CanonicalReply::Text(self.text.clone().unwrap_or_default()),
        }
    }

    /// What to record as the assistant's turn in conversation history — a short
    /// natural-language summary, not the structured payload.
    pub fn log_text(&self) -> String {
        match self.kind.as_str() {
            "choices" => self
                .prompt
                .clone()
                .unwrap_or_else(|| "[asked a question]".to_string()),
            "cart" => self
                .text
                .clone()
                .unwrap_or_else(|| "[updated the cart]".to_string()),
            _ => self.text.clone().unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_reply_maps_to_canonical_text() {
        let r = AiReply {
            kind: "text".into(),
            text: Some("hi there".into()),
            prompt: None,
            options: None,
            cart_items: None,
            total_cents: None,
            checkout_url: None,
            needs_reauth: None,
        };
        assert!(matches!(r.to_canonical(), CanonicalReply::Text(t) if t == "hi there"));
    }

    #[test]
    fn choices_reply_maps_to_canonical_choice() {
        let r = AiReply {
            kind: "choices".into(),
            text: None,
            prompt: Some("Which milk?".into()),
            options: Some(vec![
                AiChoiceOption {
                    id: "a".into(),
                    label: "Full cream".into(),
                },
                AiChoiceOption {
                    id: "b".into(),
                    label: "Low fat".into(),
                },
            ]),
            cart_items: None,
            total_cents: None,
            checkout_url: None,
            needs_reauth: None,
        };
        match r.to_canonical() {
            CanonicalReply::Choice { prompt, options } => {
                assert_eq!(prompt, "Which milk?");
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].id, "a");
            }
            other => panic!("expected choice, got {other:?}"),
        }
    }

    #[test]
    fn cart_reply_carries_items_and_checkout() {
        let r = AiReply {
            kind: "cart".into(),
            text: Some("Here's your cart".into()),
            prompt: None,
            options: None,
            cart_items: Some(vec![AiCartLine {
                name: "Milk 2L".into(),
                qty: 1,
                price_cents: Some(2999),
            }]),
            total_cents: Some(2999),
            checkout_url: Some("https://sixty60.example/checkout".into()),
            needs_reauth: None,
        };
        match r.to_canonical() {
            CanonicalReply::Cart(card) => {
                assert_eq!(card.items.len(), 1);
                assert_eq!(card.total_cents, Some(2999));
                assert!(card.checkout_url.is_some());
            }
            other => panic!("expected cart, got {other:?}"),
        }
    }
}
