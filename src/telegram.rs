//! Telegram channel adapter.
//!
//! Two responsibilities, both owned entirely here so the engine stays clean:
//!   * `parse_update` — normalize a Telegram webhook into a `CanonicalMessage`
//!     (sync, pure, natively testable)
//!   * `render` / `send` — turn a `CanonicalReply` into Telegram UI and deliver
//!     it (render is pure; `send` is wasm-only because it hits the network)

use serde_json::{Value, json};

use crate::protocol::{CanonicalMessage, CanonicalReply, ChannelKind, InboundEvent};

/// Normalize a raw Telegram `Update` body. Returns `Ok(None)` for updates we
/// don't act on (so the webhook still 200s). Both a text message and a button
/// tap collapse into the same `CanonicalMessage` shape — the engine can't tell
/// which arrived, by design.
pub fn parse_update(raw: &str) -> Result<Option<CanonicalMessage>, String> {
    let v: Value = serde_json::from_str(raw).map_err(|e| format!("bad update json: {e}"))?;

    // Button tap → Selected(option_id = callback_data).
    if let Some(cb) = v.get("callback_query") {
        let chat_id = cb
            .pointer("/message/chat/id")
            .and_then(value_id_to_string)
            .ok_or("callback_query missing chat id")?;
        let data = cb
            .get("data")
            .and_then(Value::as_str)
            .ok_or("callback_query missing data")?
            .to_string();
        return Ok(Some(CanonicalMessage {
            channel: ChannelKind::Telegram,
            user_id: chat_id,
            event: InboundEvent::Selected { option_id: data },
        }));
    }

    // Plain text message → Message.
    if let Some(message) = v.get("message") {
        let chat_id = message
            .pointer("/chat/id")
            .and_then(value_id_to_string)
            .ok_or("message missing chat id")?;
        match message.get("text").and_then(Value::as_str) {
            Some(text) => {
                return Ok(Some(CanonicalMessage {
                    channel: ChannelKind::Telegram,
                    user_id: chat_id,
                    event: InboundEvent::Message {
                        text: text.to_string(),
                    },
                }));
            }
            // Non-text messages (photos, stickers) are ignored for now.
            None => return Ok(None),
        }
    }

    Ok(None)
}

/// Telegram shows a spinner for inline-button taps until the callback query is
/// answered. Extract this separately so the Worker can acknowledge it before
/// the AI/store work runs.
pub fn callback_query_id(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw)
        .ok()?
        .pointer("/callback_query/id")?
        .as_str()
        .map(ToString::to_string)
}

/// Render engine *intent* into a Telegram `sendMessage` payload. This is where
/// "a choice" becomes "an inline keyboard" — a presentation decision the engine
/// never makes.
pub fn render(chat_id: &str, reply: &CanonicalReply) -> Value {
    match reply {
        CanonicalReply::Text(text) => json!({ "chat_id": chat_id, "text": text }),

        CanonicalReply::Choice { prompt, options } => {
            let keyboard: Vec<Value> = options
                .iter()
                .map(|o| json!([{ "text": o.label, "callback_data": o.id }]))
                .collect();
            json!({
                "chat_id": chat_id,
                "text": prompt,
                "reply_markup": { "inline_keyboard": keyboard }
            })
        }

        CanonicalReply::WebApp {
            text,
            button_label,
            url,
        } => json!({
            "chat_id": chat_id,
            "text": text,
            "reply_markup": {
                "inline_keyboard": [[{
                    "text": button_label,
                    "web_app": { "url": url }
                }]]
            }
        }),

        CanonicalReply::Cart(cart) => {
            let mut lines = vec!["🛒 Your cart:".to_string()];
            for it in &cart.items {
                lines.push(format!("• {}× {}", it.qty, it.name));
            }
            if let Some(total) = cart.total_cents {
                lines.push(format!("Total: R{:.2}", total as f64 / 100.0));
            }
            let mut payload = json!({ "chat_id": chat_id, "text": lines.join("\n") });
            // Cart hand-off: a button that opens the native store app to pay.
            if let Some(url) = &cart.checkout_url {
                payload["reply_markup"] = json!({
                    "inline_keyboard": [[{ "text": "Check out in Sixty60", "url": url }]]
                });
            }
            payload
        }
    }
}

fn value_id_to_string(v: &Value) -> Option<String> {
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_str().map(str::to_string))
}

/// Deliver a reply to Telegram. wasm-only — uses the Workers `Fetch` API.
#[cfg(target_arch = "wasm32")]
pub async fn send(bot_token: &str, chat_id: &str, reply: &CanonicalReply) -> worker::Result<()> {
    use worker::wasm_bindgen::JsValue;
    use worker::{Fetch, Headers, Method, Request, RequestInit};

    let url = format!("https://api.telegram.org/bot{bot_token}/sendMessage");
    let body = render(chat_id, reply).to_string();

    let headers = Headers::new();
    headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body)));

    let req = Request::new_with_init(&url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    if resp.status_code() >= 300 {
        let detail = resp.text().await.unwrap_or_default();
        return Err(worker::Error::RustError(format!(
            "telegram sendMessage failed ({}): {detail}",
            resp.status_code()
        )));
    }
    Ok(())
}

/// Acknowledge an inline-button tap so Telegram clears the client-side spinner.
#[cfg(target_arch = "wasm32")]
pub async fn answer_callback_query(bot_token: &str, callback_query_id: &str) -> worker::Result<()> {
    use worker::wasm_bindgen::JsValue;
    use worker::{Fetch, Headers, Method, Request, RequestInit};

    let url = format!("https://api.telegram.org/bot{bot_token}/answerCallbackQuery");
    let body = json!({ "callback_query_id": callback_query_id }).to_string();

    let headers = Headers::new();
    headers.set("content-type", "application/json")?;

    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&body)));

    let req = Request::new_with_init(&url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    if resp.status_code() >= 300 {
        let detail = resp.text().await.unwrap_or_default();
        return Err(worker::Error::RustError(format!(
            "telegram answerCallbackQuery failed ({}): {detail}",
            resp.status_code()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_message() {
        let raw = r#"{"message":{"chat":{"id":123},"text":"hi"}}"#;
        let m = parse_update(raw).unwrap().unwrap();
        assert_eq!(m.user_id, "123");
        assert!(matches!(m.event, InboundEvent::Message { text } if text == "hi"));
    }

    #[test]
    fn parses_button_tap_as_selected() {
        let raw = r#"{"callback_query":{"id":"cb-1","message":{"chat":{"id":7}},"data":"milk:a"}}"#;
        let m = parse_update(raw).unwrap().unwrap();
        assert!(matches!(m.event, InboundEvent::Selected { option_id } if option_id == "milk:a"));
        assert_eq!(callback_query_id(raw), Some("cb-1".into()));
    }

    #[test]
    fn ignores_non_text_update() {
        let raw = r#"{"message":{"chat":{"id":1},"sticker":{}}}"#;
        assert!(parse_update(raw).unwrap().is_none());
    }

    #[test]
    fn renders_choice_as_inline_keyboard() {
        let reply = CanonicalReply::choice(
            "Which milk?",
            vec![crate::protocol::Choice {
                id: "product-123".into(),
                label: "Store brand".into(),
            }],
        );
        let payload = render("9", &reply);
        assert!(payload["reply_markup"]["inline_keyboard"].is_array());
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "product-123"
        );
    }

    #[test]
    fn renders_web_app_button() {
        let reply = CanonicalReply::web_app(
            "Connect your store",
            "Connect Checkers",
            "https://example.com/telegram/login",
        );
        let payload = render("9", &reply);
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["web_app"]["url"],
            "https://example.com/telegram/login"
        );
    }
}
