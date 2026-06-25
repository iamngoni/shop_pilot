//! The canonical language the engine speaks. Pure data — no Workers runtime, no
//! channel specifics — so it compiles and unit-tests natively.
//!
//! This is the contract that keeps the channel boundary clean:
//!   * inbound  — every channel normalizes to the same `InboundEvent`
//!   * outbound — the engine emits *intent* (`CanonicalReply`), never presentation

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Telegram,
    WhatsApp,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Telegram => "telegram",
            ChannelKind::WhatsApp => "whatsapp",
        }
    }
}

/// What the user did, stripped of *how* it arrived. A button tap, a list pick,
/// and the user typing "2" all normalize to `Selected` — the engine cannot tell
/// them apart, and must not need to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundEvent {
    Message { text: String },
    Selected { option_id: String },
}

/// One normalized inbound message. `user_id` is the channel-scoped id used both
/// to key conversation state and to address the reply.
#[derive(Debug, Clone)]
pub struct CanonicalMessage {
    pub channel: ChannelKind,
    pub user_id: String,
    pub event: InboundEvent,
}

impl CanonicalMessage {
    /// Stable key for routing to this user's Durable Object / state row.
    pub fn user_key(&self) -> String {
        format!("{}:{}", self.channel.as_str(), self.user_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CartItem {
    pub name: String,
    pub qty: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CartCard {
    pub items: Vec<CartItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cents: Option<u64>,
    /// Where the user goes to confirm + pay in the native store app (cart
    /// hand-off — we never custody their money).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_url: Option<String>,
}

/// Engine output expressed as *intent*. Each adapter decides how to render it:
/// a `Choice` becomes a Telegram inline keyboard, a WhatsApp list, or a numbered
/// text fallback — without the engine changing.
#[derive(Debug, Clone)]
pub enum CanonicalReply {
    Text(String),
    Choice { prompt: String, options: Vec<Choice> },
    Cart(CartCard),
}

impl CanonicalReply {
    pub fn text(s: impl Into<String>) -> Self {
        CanonicalReply::Text(s.into())
    }

    pub fn choice(prompt: impl Into<String>, options: Vec<Choice>) -> Self {
        CanonicalReply::Choice { prompt: prompt.into(), options }
    }
}
