//! The AI engine (wasm-only). Replaces the deterministic stub: an
//! `agent-runtime` agent drives the conversation, calls grocery tools, and
//! returns a structured `AiReply` we map to a `CanonicalReply`.
//!
//! Three pieces live here:
//!   * `WorkerHttpClient` — `agent-runtime`'s `HttpClient` over `worker::Fetch`
//!   * `ShopPilotAgent`    — instructions + (mock) Sixty60 tools
//!   * `respond`           — load history → run agent → persist history → reply
//!
//! Store tools are MOCK until the Sixty60 egress spike lands; the agent, the
//! channel layer, and the structured contract are all real, so swapping mock
//! tool bodies for the real Sixty60 adapter changes nothing above this file.

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

const MODEL: &str = "claude-haiku-4-5-20251001";
const HISTORY_LIMIT: usize = 20;

const INSTRUCTIONS: &str = "\
You are Shop Pilot, a friendly grocery shopping assistant for Checkers Sixty60 in \
South Africa. Prices are in South African Rand. You help the user build a grocery \
cart by chat.

How to work:
- When the user names items they want, use `search_products` to find matching \
  products for each item.
- If a search returns several plausible matches, do NOT guess — return a reply \
  with kind \"choices\" asking the user to pick (fill `prompt` and `options`, each \
  option an {id, label}).
- Once a specific product is chosen, use `add_to_cart` with its sku_id, name, qty \
  and price_cents.
- Use `view_cart` to summarise what's in the cart.
- When the user wants to check out, call `get_checkout_link` and return a reply \
  with kind \"cart\" including cart_items, total_cents and checkout_url. We hand \
  off to the Sixty60 app for payment — you never take payment yourself.

Your FINAL answer is always a structured Reply:
- kind \"text\": a normal conversational message in `text`.
- kind \"choices\": ask the user to choose — set `prompt` and `options`.
- kind \"cart\": show the cart — set `cart_items`, `total_cents`, and `checkout_url` \
  when checking out.

Keep replies warm, concise, and natural. This is a demo running on a mock \
catalogue, so prices and products are illustrative.";

/// Top-level entry: turn one inbound message into a channel-agnostic reply.
/// Never panics out — on any failure it returns a friendly fallback so the
/// webhook still succeeds.
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
    let input = match &msg.event {
        InboundEvent::Message { text } => text.clone(),
        InboundEvent::Selected { option_id } => format!("I'll take this option: {option_id}"),
    };

    let key = format!("conv:{}", msg.user_key());
    let history = load_history(env, &key).await?;

    let api_key = env.secret("ANTHROPIC_API_KEY").map_err(werr)?.to_string();

    let agent = ShopPilotAgent::new();
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

// --- conversation history (KV) ----------------------------------------------
//
// KV is the pragmatic home for chat history today: a single user types
// sequentially, so the lack of per-key locking doesn't bite. The `UserSession`
// Durable Object remains the intended home once we need stronger consistency.

#[derive(Serialize, Deserialize)]
struct StoredMsg {
    role: String,
    text: String,
}

async fn load_history(env: &Env, key: &str) -> anyhow::Result<Vec<ChatMessage>> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv.get(key).text().await.map_err(werr)?;
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
    let value = serde_json::to_string(&stored)?;
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(key, value).map_err(werr)?.execute().await.map_err(werr)?;
    Ok(())
}

/// Convert any worker error (`worker::Error`, `worker::kv::KvError`, …) into an
/// `anyhow::Error` so the agent-runtime/anyhow result chain composes.
fn werr<E: std::fmt::Display>(e: E) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

// --- the agent + (mock) Sixty60 tools ---------------------------------------

struct CartLine {
    #[allow(dead_code)]
    sku_id: String,
    name: String,
    qty: u32,
    price_cents: u64,
}

pub struct ShopPilotAgent {
    cart: Arc<Mutex<Vec<CartLine>>>,
}

impl ShopPilotAgent {
    pub fn new() -> Self {
        Self {
            cart: Arc::new(Mutex::new(Vec::new())),
        }
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

        r.register(JsonTool::new(
            "search_products",
            "Search the Checkers Sixty60 catalogue for a grocery item. Returns candidate \
             products with sku_id, name, price_cents and stock.",
            move |_c: (), a: SearchArgs| async move { Ok(mock_search(&a.query)) },
        ));

        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "add_to_cart",
            "Add a specific product (by sku_id) to the user's cart.",
            move |_c: (), a: AddArgs| {
                let cart = cart.clone();
                async move {
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
                }
            },
        ));

        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "view_cart",
            "View the current cart contents and total.",
            move |_c: (), _a: NoArgs| {
                let cart = cart.clone();
                async move { Ok(cart_json(&cart.lock().unwrap())) }
            },
        ));

        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "get_checkout_link",
            "Get the Sixty60 hand-off link for the user to confirm and pay in the app.",
            move |_c: (), _a: NoArgs| {
                let cart = cart.clone();
                async move {
                    let total: u64 = cart
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|l| l.price_cents * l.qty as u64)
                        .sum();
                    Ok(json!({
                        "checkout_url": "https://www.sixty60.co.za/cart",
                        "total_cents": total,
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
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

// --- worker::Fetch-backed HttpClient ----------------------------------------

/// `HttpClient` impl that routes `agent-runtime`'s provider calls through the
/// Workers `Fetch` API. The non-`Send` JS futures are wrapped in `SendWrapper`
/// to satisfy the trait's `Send` bound — safe because workerd is
/// single-threaded.
pub struct WorkerHttpClient;

#[async_trait]
impl HttpClient for WorkerHttpClient {
    async fn send(&self, request: HttpRequest) -> anyhow::Result<HttpResponse> {
        SendWrapper::new(worker_fetch(request)).await
    }

    async fn send_streaming(&self, request: HttpRequest) -> anyhow::Result<HttpStreamResponse> {
        // run_structured only needs buffered `send`; we satisfy the streaming
        // contract by delivering the whole body as a single chunk.
        let resp = self.send(request).await?;
        let bytes = resp.body;
        let body: HttpByteStream =
            Box::pin(futures_util::stream::once(
                async move { Ok::<_, anyhow::Error>(bytes) },
            ));
        Ok(HttpStreamResponse {
            status: resp.status,
            body,
        })
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
