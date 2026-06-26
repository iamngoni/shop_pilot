//! The AI engine (wasm-only): an `agent-runtime` agent drives the conversation,
//! calls grocery tools, and returns a structured `AiReply` → `CanonicalReply`.
//!
//! Two paths run before/around the LLM:
//!   * **Login** — a deterministic phone→OTP→birth-date state machine (handled
//!     outside the LLM; structured input, not chat). On success the Sixty60
//!     session cookies are stored per user.
//!   * **Tools** — when the user has a session, search/cart hit the **real**
//!     Sixty60 client (`sixty60.rs`); otherwise they use mock data so the demo
//!     works logged-out. The real path is UNTESTED until a first live login.

use std::sync::{Arc, Mutex};

use agent_runtime::{
    Agent, AgentProviderKind, ChatMessage, HttpByteStream, HttpClient, HttpMethod, HttpRequest,
    HttpResponse, HttpStreamResponse, JsonTool, Llm, MessageRole, ToolRegistry,
};
use async_trait::async_trait;
use schemars::JsonSchema;
use send_wrapper::SendWrapper;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::Env;

use crate::protocol::{CanonicalMessage, CanonicalReply, InboundEvent};
use crate::reply::AiReply;
use crate::sixty60::Sixty60Client;

const MODEL: &str = "claude-haiku-4-5-20251001";
const HISTORY_LIMIT: usize = 20;

const INSTRUCTIONS: &str = "\
You are Shop Pilot, a friendly grocery shopping assistant for Checkers Sixty60 in \
South Africa. Prices are in South African Rand. You help the user build a grocery \
cart by chat.

How to work:
- Use `search_products` to find matching products for each item the user names.
- If a search returns several plausible matches, do NOT guess — return a reply \
  with kind \"choices\" asking the user to pick (fill `prompt` and `options`).
- Once a product is chosen, use `add_to_cart` with its sku_id, name, qty and \
  price_cents.
- Use `view_cart` to summarise the cart.
- When the user wants to check out, call `get_checkout_link` and return a reply \
  with kind \"cart\" including cart_items, total_cents and checkout_url. We hand \
  off to the Sixty60 app for payment — you never take payment yourself.
- If a tool reports the user isn't connected, tell them to type \"login\" to \
  connect their Checkers Sixty60 account.

Your FINAL answer is always a structured Reply (kind \"text\", \"choices\", or \
\"cart\"). Keep replies warm and concise.";

/// Top-level entry: one inbound message → one channel-agnostic reply. Never
/// panics out — any failure returns a friendly fallback.
pub async fn respond(env: &Env, msg: &CanonicalMessage) -> CanonicalReply {
    match respond_inner(env, msg).await {
        Ok(reply) => reply,
        Err(err) => {
            worker::console_error!("ai respond failed: {err:#}");
            CanonicalReply::text("Sorry — I hit a snag just now. Mind trying that again?")
        }
    }
}

async fn respond_inner(env: &Env, msg: &CanonicalMessage) -> anyhow::Result<CanonicalReply> {
    let key = msg.user_key();
    let input = match &msg.event {
        InboundEvent::Message { text } => text.clone(),
        InboundEvent::Selected { option_id } => format!("I'll take this option: {option_id}"),
    };

    // 1. Deterministic login flow takes priority over the LLM.
    if let Some(reply) = handle_login(env, &key, &input).await? {
        return Ok(reply);
    }

    // 2. Agent path. A stored session (if any) flips tools to the real store.
    let session = load_session(env, &key).await?;
    let history = load_history(env, &key).await?;
    let api_key = env.secret("ANTHROPIC_API_KEY").map_err(werr)?.to_string();

    let agent = ShopPilotAgent::new(session);
    let llm = Llm::builder()
        .provider(AgentProviderKind::Anthropic)
        .api_key(api_key)
        .with_http_client(WorkerHttpClient)
        .build()?;

    let reply: AiReply = llm
        .run_structured_with_history(&agent, &history, input.clone())
        .await?;
    let canonical = reply.to_canonical();

    let mut updated = history;
    updated.push(ChatMessage::user(input));
    updated.push(ChatMessage::assistant(reply.log_text()));
    save_history(env, &key, &updated).await?;

    Ok(canonical)
}

// --- login state machine (phone → OTP → birth-date) -------------------------

#[derive(Default, Serialize, Deserialize)]
struct LoginState {
    stage: String, // "number" | "otp" | "dob"
    mobile: String,
    otp_ref: String,
    cookies: String,
}

/// Returns `Some(reply)` if the message was consumed by the login flow, else
/// `None` (let the agent handle it).
async fn handle_login(
    env: &Env,
    key: &str,
    input: &str,
) -> anyhow::Result<Option<CanonicalReply>> {
    let trimmed = input.trim();
    let state = load_login_state(env, key).await?;

    // Not currently logging in: only start on an explicit command.
    let Some(mut st) = state else {
        if trimmed.eq_ignore_ascii_case("login") || trimmed.eq_ignore_ascii_case("/login") {
            save_login_state(
                env,
                key,
                &LoginState { stage: "number".into(), ..Default::default() },
            )
            .await?;
            return Ok(Some(CanonicalReply::text(
                "Let's connect your Checkers Sixty60 account. What's your mobile number? (e.g. 0712345678)",
            )));
        }
        return Ok(None);
    };

    if trimmed.eq_ignore_ascii_case("cancel") {
        clear_login_state(env, key).await?;
        return Ok(Some(CanonicalReply::text("No problem — login cancelled.")));
    }

    match st.stage.as_str() {
        "number" => {
            let number = normalize_msisdn(trimmed);
            let mut client = Sixty60Client::new();
            match client.request_otp(&number).await {
                Ok(otp_ref) => {
                    let next = LoginState {
                        stage: "otp".into(),
                        mobile: number,
                        otp_ref,
                        cookies: client.session().to_string(),
                    };
                    save_login_state(env, key, &next).await?;
                    Ok(Some(CanonicalReply::text(
                        "I've sent an OTP to your phone. What's the code?",
                    )))
                }
                Err(e) => {
                    clear_login_state(env, key).await?;
                    worker::console_warn!("request_otp failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "I couldn't start the login just now. Type \"login\" to try again.",
                    )))
                }
            }
        }
        "otp" => {
            let mut client = Sixty60Client::with_session(st.cookies.clone());
            match client.verify_otp(&st.mobile, trimmed, &st.otp_ref).await {
                Ok(()) => {
                    st.stage = "dob".into();
                    st.cookies = client.session().to_string();
                    save_login_state(env, key, &st).await?;
                    Ok(Some(CanonicalReply::text(
                        "Great — one last step. What's your date of birth? (YYYY-MM-DD)",
                    )))
                }
                Err(e) => {
                    worker::console_warn!("verify_otp failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "That code didn't work. Try the code again, or type \"cancel\".",
                    )))
                }
            }
        }
        "dob" => {
            let mut client = Sixty60Client::with_session(st.cookies.clone());
            match client.verify_date_of_birth(trimmed).await {
                Ok(()) => {
                    save_session(env, key, client.session()).await?;
                    clear_login_state(env, key).await?;
                    Ok(Some(CanonicalReply::text(
                        "✅ You're connected to Checkers Sixty60! Tell me what you'd like to buy.",
                    )))
                }
                Err(e) => {
                    worker::console_warn!("verify_dob failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "I couldn't verify that. Send your date of birth as YYYY-MM-DD, or \"cancel\".",
                    )))
                }
            }
        }
        _ => {
            clear_login_state(env, key).await?;
            Ok(None)
        }
    }
}

/// Normalize a SA mobile number to international form (27XXXXXXXXX).
fn normalize_msisdn(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if let Some(rest) = digits.strip_prefix("27") {
        format!("27{rest}")
    } else if let Some(rest) = digits.strip_prefix('0') {
        format!("27{rest}")
    } else {
        digits
    }
}

// --- KV state ---------------------------------------------------------------

async fn load_login_state(env: &Env, key: &str) -> anyhow::Result<Option<LoginState>> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv.get(&format!("login:{key}")).text().await.map_err(werr)?;
    Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
}

async fn save_login_state(env: &Env, key: &str, st: &LoginState) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(&format!("login:{key}"), serde_json::to_string(st)?)
        .map_err(werr)?
        .execute()
        .await
        .map_err(werr)?;
    Ok(())
}

async fn clear_login_state(env: &Env, key: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.delete(&format!("login:{key}")).await.map_err(werr)?;
    Ok(())
}

async fn load_session(env: &Env, key: &str) -> anyhow::Result<Option<String>> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv.get(&format!("sess:{key}")).text().await.map_err(werr)?;
    Ok(raw.filter(|s| !s.is_empty()))
}

async fn save_session(env: &Env, key: &str, cookies: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(&format!("sess:{key}"), cookies)
        .map_err(werr)?
        .execute()
        .await
        .map_err(werr)?;
    Ok(())
}

// --- conversation history (KV) ----------------------------------------------

#[derive(Serialize, Deserialize)]
struct StoredMsg {
    role: String,
    text: String,
}

async fn load_history(env: &Env, key: &str) -> anyhow::Result<Vec<ChatMessage>> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv.get(&format!("conv:{key}")).text().await.map_err(werr)?;
    let stored: Vec<StoredMsg> = match raw {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => Vec::new(),
    };
    Ok(stored
        .into_iter()
        .map(|m| match m.role.as_str() {
            "assistant" => ChatMessage::assistant(m.text),
            _ => ChatMessage::user(m.text),
        })
        .collect())
}

async fn save_history(env: &Env, key: &str, history: &[ChatMessage]) -> anyhow::Result<()> {
    let start = history.len().saturating_sub(HISTORY_LIMIT);
    let stored: Vec<StoredMsg> = history[start..]
        .iter()
        .map(|m| StoredMsg {
            role: match m.role {
                MessageRole::Assistant => "assistant".to_string(),
                _ => "user".to_string(),
            },
            text: m.content.clone().unwrap_or_default(),
        })
        .collect();
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(&format!("conv:{key}"), serde_json::to_string(&stored)?)
        .map_err(werr)?
        .execute()
        .await
        .map_err(werr)?;
    Ok(())
}

fn werr<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

// --- the agent + tools (real when a session exists, else mock) --------------

struct CartLine {
    #[allow(dead_code)]
    sku_id: String,
    name: String,
    qty: u32,
    price_cents: u64,
}

pub struct ShopPilotAgent {
    /// Sixty60 session cookies, if the user is connected. `None` → mock tools.
    session: Option<String>,
    cart: Arc<Mutex<Vec<CartLine>>>,
}

impl ShopPilotAgent {
    pub fn new(session: Option<String>) -> Self {
        Self { session, cart: Arc::new(Mutex::new(Vec::new())) }
    }
}

#[derive(Deserialize, JsonSchema)]
struct SearchArgs {
    /// The grocery item to look for, e.g. "2L full cream milk".
    query: String,
}

#[derive(Deserialize, JsonSchema)]
struct AddArgs {
    sku_id: String,
    name: String,
    qty: u32,
    price_cents: u64,
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

impl Agent for ShopPilotAgent {
    fn instructions(&self) -> String {
        INSTRUCTIONS.to_string()
    }

    fn model(&self) -> String {
        MODEL.to_string()
    }

    fn tools(&self) -> ToolRegistry<()> {
        let mut r = ToolRegistry::new();

        // search_products — real store when connected, else mock.
        let session = self.session.clone();
        r.register(JsonTool::new(
            "search_products",
            "Search the Checkers Sixty60 catalogue for a grocery item. Returns candidate \
             products with sku_id, name, price_cents and stock.",
            move |_c: (), a: SearchArgs| {
                let session = session.clone();
                SendWrapper::new(async move {
                    match &session {
                        Some(cookies) => {
                            let mut client = Sixty60Client::with_session(cookies.clone());
                            match client.search(&a.query).await {
                                Ok(products) => Ok(json!({
                                    "products": products.iter().map(|p| json!({
                                        "sku_id": p.product_id,
                                        "name": p.name,
                                        "brand": p.brand,
                                        "price_cents": p.price_cents,
                                        "in_stock": p.in_stock,
                                    })).collect::<Vec<_>>()
                                })),
                                Err(e) => Ok(json!({ "error": format!("{e:?}"), "products": [] })),
                            }
                        }
                        None => Ok(mock_search(&a.query)),
                    }
                })
            },
        ));

        // add_to_cart — real store when connected, else local mock cart.
        let session = self.session.clone();
        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "add_to_cart",
            "Add a specific product (by sku_id) to the user's cart.",
            move |_c: (), a: AddArgs| {
                let session = session.clone();
                let cart = cart.clone();
                SendWrapper::new(async move {
                    if let Some(cookies) = &session {
                        let mut client = Sixty60Client::with_session(cookies.clone());
                        return match client.add_to_cart(&a.sku_id, a.qty.max(1)).await {
                            Ok(()) => Ok(json!({ "ok": true })),
                            Err(e) => Ok(json!({ "ok": false, "error": format!("{e:?}") })),
                        };
                    }
                    let size = {
                        let mut c = cart.lock().unwrap();
                        c.push(CartLine {
                            sku_id: a.sku_id,
                            name: a.name,
                            qty: a.qty.max(1),
                            price_cents: a.price_cents,
                        });
                        c.len()
                    };
                    Ok(json!({ "ok": true, "cart_size": size }))
                })
            },
        ));

        // view_cart — real store when connected, else local mock cart.
        let session = self.session.clone();
        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "view_cart",
            "View the current cart contents and total.",
            move |_c: (), _a: NoArgs| {
                let session = session.clone();
                let cart = cart.clone();
                SendWrapper::new(async move {
                    if let Some(cookies) = &session {
                        let mut client = Sixty60Client::with_session(cookies.clone());
                        return match client.fetch_cart().await {
                            Ok(v) => Ok(v),
                            Err(e) => Ok(json!({ "error": format!("{e:?}") })),
                        };
                    }
                    Ok(cart_json(&cart.lock().unwrap()))
                })
            },
        ));

        // get_checkout_link — hand-off URL.
        let session = self.session.clone();
        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "get_checkout_link",
            "Get the Sixty60 hand-off link for the user to confirm and pay in the app.",
            move |_c: (), _a: NoArgs| {
                let connected = session.is_some();
                let cart = cart.clone();
                async move {
                    let total: u64 = cart.lock().unwrap().iter().map(|l| l.price_cents * l.qty as u64).sum();
                    Ok(json!({
                        "checkout_url": "https://www.checkers.co.za/cart",
                        "total_cents": if connected { Value::Null } else { json!(total) },
                    }))
                }
            },
        ));

        r
    }
}

fn mock_search(query: &str) -> Value {
    let q = query.trim();
    let s = slug(q);
    json!({
        "products": [
            { "sku_id": format!("sku-{s}-a"), "name": format!("{q} — Checkers brand"), "price_cents": 2999, "in_stock": true },
            { "sku_id": format!("sku-{s}-b"), "name": format!("{q} — premium"), "price_cents": 4599, "in_stock": true },
        ]
    })
}

fn cart_json(lines: &[CartLine]) -> Value {
    let items: Vec<Value> = lines
        .iter()
        .map(|l| json!({ "name": l.name, "qty": l.qty, "price_cents": l.price_cents }))
        .collect();
    let total: u64 = lines.iter().map(|l| l.price_cents * l.qty as u64).sum();
    json!({ "items": items, "total_cents": total })
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect()
}

// --- worker::Fetch-backed HttpClient (for agent-runtime / Anthropic) --------

/// `HttpClient` impl routing provider calls through Workers `Fetch`. The
/// non-`Send` JS futures are wrapped in `SendWrapper` to satisfy the trait's
/// `Send` bound — safe because workerd is single-threaded.
pub struct WorkerHttpClient;

#[async_trait]
impl HttpClient for WorkerHttpClient {
    async fn send(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
        SendWrapper::new(worker_fetch(request)).await
    }

    async fn send_streaming(&self, request: HttpRequest) -> anyhow::Result<HttpStreamResponse> {
        let resp = self.send(request).await?;
        let bytes = resp.body;
        let body: HttpByteStream =
            Box::pin(futures_util::stream::once(async move { Ok::<_, anyhow::Error>(bytes) }));
        Ok(HttpStreamResponse { status: resp.status, body })
    }
}

async fn worker_fetch(request: HttpRequest) -> anyhow::Result<HttpResponse> {
    use worker::wasm_bindgen::JsValue;
    use worker::{Fetch, Headers, Method, Request as WorkerRequest, RequestInit};

    let method = match request.method {
        HttpMethod::Get => Method::Get,
        HttpMethod::Post => Method::Post,
        HttpMethod::Put => Method::Put,
        HttpMethod::Patch => Method::Patch,
        HttpMethod::Delete => Method::Delete,
    };

    let headers = Headers::new();
    for (name, value) in &request.headers {
        headers.set(name, value).map_err(werr)?;
    }

    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if !request.body.is_empty() {
        let body_str =
            String::from_utf8(request.body).map_err(|e| anyhow::anyhow!("request body utf8: {e}"))?;
        init.with_body(Some(JsValue::from_str(&body_str)));
    }

    let req = WorkerRequest::new_with_init(&request.url, &init).map_err(werr)?;
    let mut resp = Fetch::Request(req).send().await.map_err(werr)?;
    let status = resp.status_code();
    let body = resp.bytes().await.map_err(werr)?;
    Ok(HttpResponse { status, body })
}
