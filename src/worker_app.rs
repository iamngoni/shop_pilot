//! Workers entrypoint (wasm-only). Receives channel webhooks, routes each into
//! the engine, and sends the engine's reply back through the originating
//! channel adapter. The full inbound → engine → outbound loop, end to end.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use worker::{Context, Env, Request, Response, Result, Router, event};

use crate::protocol::CanonicalReply;
use crate::{ai, telegram};

const TELEGRAM_BOT_TOKEN: &str = "TELEGRAM_BOT_TOKEN";
type HmacSha256 = Hmac<Sha256>;

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    Router::new()
        .get("/health", |_, _| Response::ok("ok"))
        .get("/telegram/login", |_, _| {
            Response::from_html(TELEGRAM_LOGIN_HTML)
        })
        .post_async("/telegram/login/start", handle_web_login_start)
        .post_async("/telegram/login/otp", handle_web_login_otp)
        .post_async("/telegram/login/consent", handle_web_login_consent)
        .post_async("/telegram/login/dob", handle_web_login_dob)
        .post_async("/telegram/login/identity", handle_web_login_identity)
        .post_async("/telegram/login/cancel", handle_web_login_cancel)
        .post_async("/webhook/telegram", handle_telegram_webhook)
        .run(req, env)
        .await
}

async fn handle_telegram_webhook(
    mut req: Request,
    ctx: worker::RouteContext<()>,
) -> Result<Response> {
    let raw = req.text().await?;
    let callback_query_id = telegram::callback_query_id(&raw);

    let msg = match telegram::parse_update(&raw) {
        Ok(Some(msg)) => msg,
        // Nothing actionable in this update — acknowledge so Telegram stops
        // retrying.
        Ok(None) => return Response::ok("ignored"),
        Err(e) => {
            worker::console_warn!("telegram parse failed: {e}");
            return Response::ok("ignored");
        }
    };

    let token = ctx
        .secret(TELEGRAM_BOT_TOKEN)
        .map(|s| s.to_string())
        .map_err(|_| worker::Error::RustError("TELEGRAM_BOT_TOKEN not set".into()))?;

    if let Some(callback_query_id) = callback_query_id {
        if let Err(e) = telegram::answer_callback_query(&token, &callback_query_id).await {
            worker::console_error!("telegram answerCallbackQuery failed: {e}");
        }
    }

    // AI engine: intent in, intent out. Loads/persists conversation history and
    // runs the agent, returning a structured reply mapped to CanonicalReply.
    let reply = ai::respond(&ctx.env, &msg).await;
    // Observability during live validation — surfaces the bot's reply in tail
    // even when the Telegram send fails (e.g. synthetic test chat).
    worker::console_log!("[reply -> {}] {:?}", msg.user_id, reply);

    if let Err(e) = telegram::send(&token, &msg.user_id, &reply).await {
        worker::console_error!("telegram send failed: {e}");
    }

    Response::ok("ok")
}

#[derive(Deserialize)]
struct WebLoginRequest {
    init_data: String,
    #[serde(default)]
    phone: String,
    #[serde(default)]
    otp: String,
    #[serde(default)]
    date_of_birth: String,
    #[serde(default)]
    identity: String,
}

async fn handle_web_login_start(req: Request, ctx: worker::RouteContext<()>) -> Result<Response> {
    let (body, telegram_user_id) = match verified_web_login_request(req, &ctx).await {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };
    let response = ai::web_login_start(&ctx.env, &telegram_user_id, &body.phone)
        .await
        .map_err(worker_error)?;
    web_login_response(&ctx, &telegram_user_id, response).await
}

async fn handle_web_login_otp(req: Request, ctx: worker::RouteContext<()>) -> Result<Response> {
    let (body, telegram_user_id) = match verified_web_login_request(req, &ctx).await {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };
    let response = ai::web_login_otp(&ctx.env, &telegram_user_id, &body.otp)
        .await
        .map_err(worker_error)?;
    web_login_response(&ctx, &telegram_user_id, response).await
}

async fn handle_web_login_consent(req: Request, ctx: worker::RouteContext<()>) -> Result<Response> {
    let (_body, telegram_user_id) = match verified_web_login_request(req, &ctx).await {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };
    let response = ai::web_login_accept_consent(&ctx.env, &telegram_user_id)
        .await
        .map_err(worker_error)?;
    web_login_response(&ctx, &telegram_user_id, response).await
}

async fn handle_web_login_dob(req: Request, ctx: worker::RouteContext<()>) -> Result<Response> {
    let (body, telegram_user_id) = match verified_web_login_request(req, &ctx).await {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };
    let response = ai::web_login_dob(&ctx.env, &telegram_user_id, &body.date_of_birth)
        .await
        .map_err(worker_error)?;
    web_login_response(&ctx, &telegram_user_id, response).await
}

async fn handle_web_login_identity(
    req: Request,
    ctx: worker::RouteContext<()>,
) -> Result<Response> {
    let (body, telegram_user_id) = match verified_web_login_request(req, &ctx).await {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };
    let response = ai::web_login_identity(&ctx.env, &telegram_user_id, &body.identity)
        .await
        .map_err(worker_error)?;
    web_login_response(&ctx, &telegram_user_id, response).await
}

async fn handle_web_login_cancel(req: Request, ctx: worker::RouteContext<()>) -> Result<Response> {
    let (_body, telegram_user_id) = match verified_web_login_request(req, &ctx).await {
        Ok(request) => request,
        Err(response) => return Ok(response),
    };
    ai::web_login_cancel(&ctx.env, &telegram_user_id)
        .await
        .map_err(worker_error)?;
    Response::from_json(&json!({
        "ok": true,
        "step": "cancelled",
        "message": "Login cancelled."
    }))
}

async fn verified_web_login_request(
    mut req: Request,
    ctx: &worker::RouteContext<()>,
) -> std::result::Result<(WebLoginRequest, String), Response> {
    let body: WebLoginRequest = match req.json().await {
        Ok(body) => body,
        Err(_) => return Err(json_error_response("Invalid request body.", 400)),
    };
    let token = ctx
        .secret(TELEGRAM_BOT_TOKEN)
        .map(|s| s.to_string())
        .map_err(|_| {
            json_error_response("Telegram bot token is not configured on the server.", 500)
        })?;
    let telegram_user_id = validate_telegram_init_data(&body.init_data, &token)
        .map_err(|_| json_error_response("Open this form from Telegram.", 401))?;
    Ok((body, telegram_user_id))
}

fn json_error_response(message: &str, status: u16) -> Response {
    Response::from_json(&json!({
        "ok": false,
        "step": "error",
        "message": message,
    }))
    .map(|response| response.with_status(status))
    .unwrap_or_else(|_| Response::error(message, status).expect("valid error status"))
}

async fn web_login_response(
    ctx: &worker::RouteContext<()>,
    telegram_user_id: &str,
    response: ai::WebLoginResponse,
) -> Result<Response> {
    if response.ok && response.step == "complete" {
        let token = ctx
            .secret(TELEGRAM_BOT_TOKEN)
            .map(|s| s.to_string())
            .map_err(|_| worker::Error::RustError("TELEGRAM_BOT_TOKEN not set".into()))?;
        let reply = match ai::post_web_login_reply(&ctx.env, telegram_user_id).await {
            Ok(reply) => reply,
            Err(e) => {
                worker::console_error!("post web login reply failed: {e:#}");
                CanonicalReply::text(
                    "You're connected to Checkers Sixty60. Tell me what you'd like to buy.",
                )
            }
        };
        if let Err(e) = telegram::send(&token, telegram_user_id, &reply).await {
            worker::console_error!("telegram send after web login failed: {e}");
        }
    }
    Response::from_json(&response)
}

fn validate_telegram_init_data(init_data: &str, bot_token: &str) -> Result<String> {
    let mut fields = Vec::new();
    for pair in init_data.split('&').filter(|p| !p.is_empty()) {
        let mut parts = pair.splitn(2, '=');
        let key = url_decode(parts.next().unwrap_or_default())?;
        let value = url_decode(parts.next().unwrap_or_default())?;
        fields.push((key, value));
    }

    let hash = fields
        .iter()
        .find(|(key, _)| key == "hash")
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| worker::Error::RustError("telegram init data missing hash".into()))?;

    let auth_date = fields
        .iter()
        .find(|(key, _)| key == "auth_date")
        .and_then(|(_, value)| value.parse::<f64>().ok())
        .ok_or_else(|| worker::Error::RustError("telegram init data missing auth_date".into()))?;
    let now = js_sys::Date::now() / 1000.0;
    if now - auth_date > 86_400.0 {
        return Err(worker::Error::RustError(
            "telegram init data expired".into(),
        ));
    }

    let mut check_fields: Vec<_> = fields
        .iter()
        .filter(|(key, _)| key != "hash")
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    check_fields.sort();
    let data_check_string = check_fields.join("\n");

    let mut secret_mac = HmacSha256::new_from_slice(b"WebAppData")
        .map_err(|e| worker::Error::RustError(e.to_string()))?;
    secret_mac.update(bot_token.as_bytes());
    let secret = secret_mac.finalize().into_bytes();

    let mut mac =
        HmacSha256::new_from_slice(&secret).map_err(|e| worker::Error::RustError(e.to_string()))?;
    mac.update(data_check_string.as_bytes());
    let expected_hash = bytes_to_hex(&mac.finalize().into_bytes());
    if !constant_time_eq(hash.as_bytes(), expected_hash.as_bytes()) {
        return Err(worker::Error::RustError(
            "telegram init data signature mismatch".into(),
        ));
    }

    let user = fields
        .iter()
        .find(|(key, _)| key == "user")
        .map(|(_, value)| value)
        .ok_or_else(|| worker::Error::RustError("telegram init data missing user".into()))?;
    let user: Value = serde_json::from_str(user).map_err(worker_error)?;
    user.get("id")
        .and_then(|id| {
            id.as_i64()
                .map(|id| id.to_string())
                .or_else(|| id.as_u64().map(|id| id.to_string()))
                .or_else(|| id.as_str().map(ToString::to_string))
        })
        .ok_or_else(|| worker::Error::RustError("telegram init data missing user id".into()))
}

fn url_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .map_err(|e| worker::Error::RustError(e.to_string()))?;
                let byte = u8::from_str_radix(hex, 16)
                    .map_err(|e| worker::Error::RustError(e.to_string()))?;
                out.push(byte);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|e| worker::Error::RustError(e.to_string()))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn worker_error<E: std::fmt::Display>(e: E) -> worker::Error {
    worker::Error::RustError(e.to_string())
}

const TELEGRAM_LOGIN_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
  <title>Connect Checkers</title>
  <script src="https://telegram.org/js/telegram-web-app.js"></script>
  <style>
    :root {
      color-scheme: light dark;
      --bg: var(--tg-theme-bg-color, #0f1720);
      --text: var(--tg-theme-text-color, #f8fafc);
      --muted: var(--tg-theme-hint-color, #93a4b7);
      --button: var(--tg-theme-button-color, #22c55e);
      --button-text: var(--tg-theme-button-text-color, #ffffff);
      --panel: rgba(148, 163, 184, 0.14);
      --border: rgba(148, 163, 184, 0.28);
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      min-height: 100vh;
      background: var(--bg);
      color: var(--text);
      display: grid;
      place-items: start center;
      padding: max(20px, env(safe-area-inset-top)) 18px max(24px, env(safe-area-inset-bottom));
    }
    main {
      width: min(100%, 460px);
      padding-top: 10px;
    }
    h1 {
      font-size: 26px;
      line-height: 1.12;
      margin: 0 0 8px;
      letter-spacing: 0;
    }
    p {
      margin: 0;
      color: var(--muted);
      font-size: 15px;
      line-height: 1.42;
    }
    .panel {
      margin-top: 22px;
      border: 1px solid var(--border);
      background: var(--panel);
      border-radius: 8px;
      padding: 18px;
    }
    label {
      display: block;
      font-size: 13px;
      color: var(--muted);
      margin-bottom: 8px;
    }
    input {
      width: 100%;
      border: 1px solid var(--border);
      border-radius: 8px;
      padding: 14px 13px;
      font: inherit;
      color: var(--text);
      background: rgba(255, 255, 255, 0.06);
      outline: none;
    }
    input:focus { border-color: var(--button); }
    button {
      width: 100%;
      min-height: 48px;
      margin-top: 14px;
      border: 0;
      border-radius: 8px;
      background: var(--button);
      color: var(--button-text);
      font: inherit;
      font-weight: 700;
    }
    button.secondary {
      background: transparent;
      color: var(--muted);
      border: 1px solid var(--border);
    }
    .message {
      min-height: 24px;
      margin-top: 14px;
      color: var(--text);
      font-size: 14px;
    }
    .error { color: #ef4444; }
    .hidden { display: none; }
  </style>
</head>
<body>
  <main>
    <h1>Connect Checkers</h1>
    <p id="intro">Sign in with your Checkers Sixty60 phone number. Your OTP and profile checks stay inside this secure Telegram screen.</p>
    <section class="panel">
      <form id="form">
        <div id="field-wrap">
          <label id="label" for="value">Mobile number</label>
          <input id="value" name="value" type="tel" inputmode="tel" autocomplete="tel" placeholder="0712345678">
        </div>
        <button id="submit" type="submit">Send OTP</button>
        <button id="cancel" class="secondary" type="button">Cancel</button>
        <p id="message" class="message"></p>
      </form>
    </section>
  </main>
  <script>
    const tg = window.Telegram && window.Telegram.WebApp;
    if (tg) {
      tg.ready();
      tg.expand();
    }

    const initData = tg ? tg.initData : "";
    const form = document.getElementById("form");
    const fieldWrap = document.getElementById("field-wrap");
    const label = document.getElementById("label");
    const input = document.getElementById("value");
    const submit = document.getElementById("submit");
    const cancel = document.getElementById("cancel");
    const message = document.getElementById("message");
    let step = "phone";

    function setMessage(text, isError = false) {
      message.textContent = text || "";
      message.className = isError ? "message error" : "message";
    }

    function setStep(next, text) {
      step = next;
      input.value = "";
      fieldWrap.classList.remove("hidden");
      cancel.classList.remove("hidden");
      input.disabled = false;
      if (next === "otp") {
        label.textContent = "OTP code";
        input.type = "text";
        input.inputMode = "numeric";
        input.placeholder = "123456";
        submit.textContent = "Verify OTP";
      } else if (next === "consent") {
        fieldWrap.classList.add("hidden");
        submit.textContent = "Accept and continue";
      } else if (next === "dob") {
        label.textContent = "Date of birth";
        input.type = "text";
        input.inputMode = "numeric";
        input.placeholder = "DD/MM/YYYY";
        submit.textContent = "Continue";
      } else if (next === "identity") {
        label.textContent = "SA ID or passport";
        input.type = "text";
        input.inputMode = "text";
        input.placeholder = "9001015009086";
        submit.textContent = "Continue";
      } else if (next === "complete") {
        fieldWrap.classList.add("hidden");
        cancel.classList.add("hidden");
        submit.textContent = "Close";
      }
      setMessage(text || "");
      input.focus();
    }

    async function post(action, payload = {}) {
      submit.disabled = true;
      setMessage("Working...");
      try {
        const res = await fetch(`/telegram/login/${action}`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ init_data: initData, ...payload })
        });
        if (!res.ok) throw new Error("Could not verify this Telegram session.");
        return await res.json();
      } finally {
        submit.disabled = false;
      }
    }

    async function applyResponse(res) {
      if (!res.ok) {
        setMessage(res.message || "Something went wrong.", true);
        return;
      }
      setStep(res.step, res.message);
    }

    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      if (!initData) {
        setMessage("Open this screen from the Telegram button.", true);
        return;
      }
      if (step === "complete") {
        if (tg) tg.close();
        return;
      }
      const value = input.value.trim();
      if (step !== "consent" && !value) {
        setMessage("This field is required.", true);
        return;
      }
      const action = step === "phone" ? "start" : step;
      const payload =
        step === "phone" ? { phone: value } :
        step === "otp" ? { otp: value } :
        step === "dob" ? { date_of_birth: value } :
        step === "identity" ? { identity: value } :
        {};
      applyResponse(await post(action, payload));
    });

    cancel.addEventListener("click", async () => {
      if (initData) {
        try { await post("cancel"); } catch (_) {}
      }
      if (tg) tg.close();
    });

    if (!initData) {
      setMessage("Open this screen from the Telegram Connect Checkers button.", true);
    }
  </script>
</body>
</html>"#;
