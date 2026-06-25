//! The channel-agnostic engine. Consumes a `CanonicalMessage`, returns a
//! `CanonicalReply` (intent only). No channel specifics, no Workers runtime.
//!
//! This is a deterministic placeholder so the chat loop runs end-to-end today.
//! The real engine slots in here: per-user state (Durable Object), LLM intent +
//! item extraction, catalog matching against the store, cart orchestration.

use crate::protocol::{CanonicalMessage, CanonicalReply, Choice, InboundEvent};

/// Entry point for a single inbound turn.
///
/// Stateless for now — once the `UserSession` DO is wired, this becomes
/// `handle(state, msg)` and threads conversation context through.
pub fn handle(msg: &CanonicalMessage) -> CanonicalReply {
    match &msg.event {
        InboundEvent::Message { text } => handle_text(text),
        InboundEvent::Selected { option_id } => {
            // The user picked a disambiguation option. The real engine looks up
            // the pending choice for this user and advances the cart; for now we
            // just acknowledge so the contract is exercised end-to-end.
            CanonicalReply::text(format!("Got it — added `{option_id}`. (engine stub)"))
        }
    }
}

fn handle_text(text: &str) -> CanonicalReply {
    let items = extract_items(text);

    if items.is_empty() {
        return CanonicalReply::text(
            "Tell me what you'd like to buy — e.g. \"2 litres of milk and a loaf of bread\".",
        );
    }

    // Demonstrate the disambiguation primitive: when an item is ambiguous the
    // engine emits *intent* (a choice), and each adapter renders it natively.
    if let Some(first) = items.first()
        && is_ambiguous(first)
    {
        return CanonicalReply::choice(
            format!("Which {first}?"),
            vec![
                Choice { id: format!("{first}:a"), label: format!("{first} — store brand") },
                Choice { id: format!("{first}:b"), label: format!("{first} — name brand") },
            ],
        );
    }

    CanonicalReply::text(format!(
        "Building your cart: {}. (catalog matching not wired yet)",
        items.join(", ")
    ))
}

/// Naive item splitter standing in for LLM extraction.
fn extract_items(text: &str) -> Vec<String> {
    text.split([',', '\n'])
        .flat_map(|chunk| chunk.split(" and "))
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn is_ambiguous(item: &str) -> bool {
    // Placeholder: real ambiguity comes from how many SKUs the catalog matches.
    item.contains("milk")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChannelKind, InboundEvent};

    fn msg(text: &str) -> CanonicalMessage {
        CanonicalMessage {
            channel: ChannelKind::Telegram,
            user_id: "42".into(),
            event: InboundEvent::Message { text: text.into() },
        }
    }

    #[test]
    fn empty_input_prompts_for_items() {
        assert!(matches!(handle(&msg("")), CanonicalReply::Text(_)));
    }

    #[test]
    fn ambiguous_item_emits_choice_intent() {
        match handle(&msg("milk")) {
            CanonicalReply::Choice { options, .. } => assert_eq!(options.len(), 2),
            other => panic!("expected a choice, got {other:?}"),
        }
    }

    #[test]
    fn parses_multiple_items() {
        let items = extract_items("bread, eggs and butter");
        assert_eq!(items, vec!["bread", "eggs", "butter"]);
    }
}
