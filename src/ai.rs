//! The AI engine (wasm-only): an `agent-runtime` agent drives the conversation,
//! calls grocery tools, and returns a structured `AiReply` → `CanonicalReply`.
//!
//! Two paths run before/around the LLM:
//!   * **Login** — a deterministic phone→OTP→profile/DOB state machine (handled
//!     outside the LLM; structured input, not chat). On success the Sixty60
//!     session cookies and store context are stored per user.
//!   * **Tools** — when the user has a session, search/cart hit the **real**
//!     Sixty60 client (`sixty60.rs`); otherwise they use mock data so the demo
//!     works logged-out.

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

use crate::login_flow::{normalize_date_of_birth, normalize_msisdn, parse_identity_document};
use crate::preferences::{
    PreferenceAction, PreferenceOption, PreferenceQuestion, ShoppingMode, UserPreferences,
    parse_preference_callback,
};
use crate::protocol::{CanonicalMessage, CanonicalReply, Choice, InboundEvent};
use crate::reply::AiReply;
use crate::sixty60::{Sixty60Client, StoreError};

const MODEL: &str = "claude-haiku-4-5-20251001";
const HISTORY_LIMIT: usize = 20;

const INSTRUCTIONS: &str = "\
You are Shop Pilot, a friendly grocery shopping assistant for Checkers Sixty60 in \
South Africa. Prices are in South African Rand. You help the user build a grocery \
cart by chat.

How to work:
- Follow the current user preferences below before deciding whether to ask, \
  choose, or add.
- Use `search_products` to find matching products for each item the user names.
- In manual mode, return a reply with kind \"choices\" when there is more than \
  one plausible match, asking the user to pick (fill `prompt` and `options`).
- In auto mode, choose sensible products and use `add_to_cart` directly when \
  confidence is high. Ask a concise clarification question before adding when \
  the request, preferences, dietary constraints, quantity, price, or pantry \
  assumptions make the right choice unclear.
- In auto mode, do not return product candidate buttons. If you need more \
  information, return kind \"text\" with one short question. Otherwise choose \
  on the user's behalf, call `add_to_cart`, and then confirm what you added.
- Infer sensible quantities from the user's context. If they give a number of \
  people or an event, choose practical quantities without asking about every \
  item. For a braai, use normal serving assumptions for meat, starches, sides, \
  sauces, charcoal/firelighters, and non-alcoholic drinks. Do not add alcohol \
  unless the user explicitly asks for it.
- Never claim an item is added unless `add_to_cart` returned `ok: true` in this \
  same turn. Prior assistant messages and cart summaries are not proof of the \
  current cart.
- Never say you are still working, adding now, or need a moment. This system \
  sends one final reply per turn, so do all tool calls before the final reply.
- For each choice option, set `id` to the product's `sku_id` exactly. Do not \
  invent short ids like \"1\", \"a\", or names; button taps use `id` to update \
  the real Checkers cart.
- Once a product is chosen, use `add_to_cart` with its sku_id, name, qty and \
  price_cents.
- Use `view_cart` to summarise the cart.
- When the user wants to check out, call `get_checkout_link` and return a reply \
  with kind \"cart\" including cart_items, total_cents and checkout_url. We hand \
  off to the Sixty60 app for payment — you never take payment yourself.
- If a tool returns `needs_reauth: true`, tell them to type \"login\" to \
  reconnect their Checkers Sixty60 account.
- If a cart tool returns `cart_update_failed: true` without `needs_reauth`, do \
  not ask them to log in. Say Checkers did not accept the cart update and ask \
  them to try once more.

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

    // Button taps are already concrete product selections. Keep them out of the
    // login state machine and the LLM so stale state cannot turn a cart write
    // into another prompt.
    if let InboundEvent::Selected { option_id } = &msg.event {
        if let Some(action) = parse_preference_callback(option_id) {
            return handle_preference_selection(env, &key, action).await;
        }
        let session = load_session(env, &key).await?;
        return handle_selected_option(env, &key, session, option_id).await;
    }

    let input = match &msg.event {
        InboundEvent::Message { text } => text.clone(),
        InboundEvent::Selected { .. } => unreachable!("selected events return above"),
    };

    // 0. Explicit settings commands are deterministic and do not involve the LLM.
    if let Some(reply) = handle_preference_command(env, &key, &input).await? {
        return Ok(reply);
    }

    // 1. Deterministic login flow takes priority over the LLM.
    if let Some(reply) = handle_login(env, &key, &input).await? {
        return Ok(reply);
    }

    // 2. First-use setup captures stable preferences before shopping.
    let preferences = load_preferences(env, &key).await?;
    if let Some(reply) = ensure_preferences(env, &key, &input, preferences.clone()).await? {
        return Ok(reply);
    }

    if load_session(env, &key).await?.is_none() && should_require_store_login(&input) {
        save_pending_login_input(env, &key, &input).await?;
        return Ok(login_prompt(env));
    }

    // 3. Agent path. A stored session (if any) flips tools to the real store.
    run_agent(env, &key, input, preferences).await
}

async fn run_agent(
    env: &Env,
    key: &str,
    input: String,
    preferences: UserPreferences,
) -> anyhow::Result<CanonicalReply> {
    let session = load_session(env, &key).await?;
    let history = load_history(env, &key).await?;
    let api_key = env.secret("ANTHROPIC_API_KEY").map_err(werr)?.to_string();

    let agent = ShopPilotAgent::new(session.clone(), preferences.clone());
    let llm = Llm::builder()
        .provider(AgentProviderKind::Anthropic)
        .api_key(api_key)
        .with_http_client(WorkerHttpClient)
        .build()?;

    let mut reply_result: anyhow::Result<AiReply> = llm
        .run_structured_with_history(&agent, &history, input.clone())
        .await;
    if matches!(reply_result.as_ref(), Ok(reply) if should_retry_unverified_reply(&preferences, reply, &agent))
    {
        let correction = format!(
            "Correction: your previous reply either claimed cart work without a verified \
             add_to_cart success, returned product buttons in auto mode, or said you were \
             still working. You must complete any required search_products/add_to_cart tool \
             calls before your final answer. If add_to_cart fails or is not called, say that \
             nothing was added. In auto mode, do not return product candidate buttons; ask \
             one short text clarification only if a required detail is genuinely missing. \
             Original user request: {input}"
        );
        reply_result = llm
            .run_structured_with_history(&agent, &history, correction)
            .await;
    }
    sync_session(
        env,
        &key,
        session.as_deref(),
        agent.session_snapshot().as_deref(),
    )
    .await?;
    let reply = reply_result?;
    let successful_adds = agent.successful_adds_snapshot();
    let failed_adds = agent.failed_adds_snapshot();
    let canonical = verified_canonical_reply(&reply, &successful_adds, &failed_adds);

    let mut updated = history;
    updated.push(ChatMessage::user(input));
    updated.push(ChatMessage::assistant(canonical_log_text(
        &canonical, &reply,
    )));
    save_history(env, &key, &updated).await?;

    Ok(canonical)
}

fn should_retry_unverified_reply(
    preferences: &UserPreferences,
    reply: &AiReply,
    agent: &ShopPilotAgent,
) -> bool {
    agent.successful_adds_snapshot().is_empty()
        && (matches!(preferences.mode, Some(ShoppingMode::Auto)) && reply.kind == "choices"
            || reply_claims_cart_success(reply)
            || reply_is_working_placeholder(reply))
}

fn verified_canonical_reply(
    reply: &AiReply,
    successful_adds: &[CartWrite],
    failed_adds: &[CartWriteFailure],
) -> CanonicalReply {
    if !successful_adds.is_empty() {
        return CanonicalReply::text(successful_adds_reply(successful_adds, failed_adds));
    }
    if !failed_adds.is_empty() {
        return CanonicalReply::text(failed_adds_reply(failed_adds));
    }
    if reply_claims_cart_success(reply) || reply_is_working_placeholder(reply) {
        return CanonicalReply::text(
            "I haven't added anything yet because I could not verify a Checkers cart update. Try once more with the item you want added.",
        );
    }
    reply.to_canonical()
}

fn successful_adds_reply(
    successful_adds: &[CartWrite],
    failed_adds: &[CartWriteFailure],
) -> String {
    let mut lines = vec!["Added to your Checkers cart:".to_string()];
    lines.extend(successful_adds.iter().map(|add| {
        format!(
            "- {} x {} ({})",
            add.qty,
            add.name,
            cents_to_rand(add.price_cents)
        )
    }));
    if !failed_adds.is_empty() {
        lines.push("".to_string());
        lines.push(format!(
            "Checkers did not accept {} other cart update{}.",
            failed_adds.len(),
            if failed_adds.len() == 1 { "" } else { "s" }
        ));
    }
    lines.join("\n")
}

fn failed_adds_reply(failed_adds: &[CartWriteFailure]) -> String {
    let item = failed_adds
        .first()
        .map(|failure| failure.name.as_str())
        .unwrap_or("that item");
    format!(
        "Checkers did not accept the cart update for {item}, so I haven't added anything. Please try once more."
    )
}

fn canonical_log_text(canonical: &CanonicalReply, original: &AiReply) -> String {
    match canonical {
        CanonicalReply::Text(text) => text.clone(),
        _ => original.log_text(),
    }
}

fn reply_claims_cart_success(reply: &AiReply) -> bool {
    let text = reply_text(reply);
    let lower = text.to_ascii_lowercase();
    if lower.contains("couldn't add")
        || lower.contains("could not add")
        || lower.contains("can't add")
        || lower.contains("cannot add")
        || lower.contains("not added")
        || lower.contains("nothing was added")
        || lower.contains("didn't add")
        || lower.contains("did not add")
    {
        return false;
    }
    lower.contains("i've added")
        || lower.contains("i have added")
        || lower.contains("i added")
        || lower.contains("added ")
        || lower.contains(" to your cart")
        || lower.contains("in your cart")
        || lower.contains("cart summary")
}

fn reply_is_working_placeholder(reply: &AiReply) -> bool {
    let lower = reply_text(reply).to_ascii_lowercase();
    lower.contains("just a moment")
        || lower.contains("one moment")
        || lower.contains("let me add")
        || lower.contains("i'm adding")
        || lower.contains("i am adding")
        || lower.contains("adding these now")
        || lower.contains("adding all")
}

fn reply_text(reply: &AiReply) -> String {
    let mut parts = Vec::new();
    if let Some(text) = reply.text.as_deref() {
        parts.push(text.to_string());
    }
    if let Some(prompt) = reply.prompt.as_deref() {
        parts.push(prompt.to_string());
    }
    if let Some(options) = &reply.options {
        parts.extend(options.iter().map(|option| option.label.clone()));
    }
    parts.join("\n")
}

fn cents_to_rand(cents: u64) -> String {
    format!("R{}.{:02}", cents / 100, cents % 100)
}

async fn handle_preference_command(
    env: &Env,
    key: &str,
    input: &str,
) -> anyhow::Result<Option<CanonicalReply>> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();

    if is_start_command(trimmed) {
        let prefs = load_preferences(env, key).await?;
        if let Some(question) = prefs.next_question() {
            return Ok(Some(preference_question_reply(question)));
        }
        return Ok(Some(CanonicalReply::text(
            "Tell me what you'd like to buy.",
        )));
    }

    if lower == "/preferences reset" || lower == "/prefs reset" {
        clear_preferences(env, key).await?;
        clear_pending_preference_input(env, key).await?;
        let prefs = UserPreferences::default();
        return Ok(Some(preference_question_reply(
            prefs
                .next_question()
                .expect("default preferences are incomplete"),
        )));
    }

    if lower == "/preferences" || lower == "/prefs" {
        let prefs = load_preferences(env, key).await?;
        if let Some(question) = prefs.next_question() {
            return Ok(Some(preference_question_reply_with_intro(
                format!(
                    "Current preferences:\n{}\n\nLet's finish setup.",
                    prefs.summary()
                ),
                question,
            )));
        }
        return Ok(Some(CanonicalReply::text(format!(
            "Current preferences:\n{}",
            prefs.summary()
        ))));
    }

    if lower == "/mode" {
        let prefs = load_preferences(env, key).await?;
        return Ok(Some(mode_choice_reply(&prefs)));
    }

    if lower.starts_with("/mode ") {
        let rest = trimmed[6..].trim();
        let Some(mode) = ShoppingMode::parse(rest) else {
            return Ok(Some(CanonicalReply::text(
                "I didn't recognise that mode. Use /mode manual or /mode auto.",
            )));
        };
        let mut prefs = load_preferences(env, key).await?;
        prefs.mode = Some(mode);
        save_preferences(env, key, &prefs).await?;
        if let Some(question) = prefs.next_question() {
            return Ok(Some(preference_question_reply_with_intro(
                format!("{} mode is on. One more setup question:", mode.label()),
                question,
            )));
        }
        return Ok(Some(CanonicalReply::text(format!(
            "{} mode is on.\n{}",
            mode.label(),
            prefs.summary()
        ))));
    }

    Ok(None)
}

async fn ensure_preferences(
    env: &Env,
    key: &str,
    input: &str,
    mut prefs: UserPreferences,
) -> anyhow::Result<Option<CanonicalReply>> {
    if prefs.is_complete() {
        return Ok(None);
    }

    if prefs.apply_text_for_next_question(input) {
        save_preferences(env, key, &prefs).await?;
        if let Some(question) = prefs.next_question() {
            return Ok(Some(preference_question_reply(question)));
        }
        if let Some(pending) = load_pending_preference_input(env, key).await? {
            clear_pending_preference_input(env, key).await?;
            return Ok(Some(run_agent(env, key, pending, prefs).await?));
        }
        return Ok(Some(CanonicalReply::text(format!(
            "Preferences saved.\n{}\n\nTell me what you'd like to buy.",
            prefs.summary(),
        ))));
    }

    if !is_setup_only_input(input) && load_pending_preference_input(env, key).await?.is_none() {
        save_pending_preference_input(env, key, input).await?;
    }

    let question = prefs
        .next_question()
        .expect("incomplete preferences must have a next question");
    Ok(Some(preference_question_reply(question)))
}

async fn handle_preference_selection(
    env: &Env,
    key: &str,
    action: PreferenceAction,
) -> anyhow::Result<CanonicalReply> {
    let mut prefs = load_preferences(env, key).await?;
    prefs.apply_action(action);
    save_preferences(env, key, &prefs).await?;

    if let Some(question) = prefs.next_question() {
        return Ok(preference_question_reply(question));
    }

    if let Some(pending) = load_pending_preference_input(env, key).await? {
        clear_pending_preference_input(env, key).await?;
        return run_agent(env, key, pending, prefs).await;
    }

    Ok(CanonicalReply::text(format!(
        "Preferences saved.\n{}",
        prefs.summary()
    )))
}

fn mode_choice_reply(prefs: &UserPreferences) -> CanonicalReply {
    let current = prefs.mode.map(ShoppingMode::label).unwrap_or("not set");
    CanonicalReply::choice(
        format!("Current mode: {current}. Choose how I should shop."),
        vec![
            Choice {
                id: "pref:mode:manual".to_string(),
                label: "Manual".to_string(),
            },
            Choice {
                id: "pref:mode:auto".to_string(),
                label: "Auto".to_string(),
            },
        ],
    )
}

fn preference_question_reply(question: PreferenceQuestion) -> CanonicalReply {
    CanonicalReply::choice(question.prompt, preference_choices(question.options))
}

fn preference_question_reply_with_intro(
    intro: impl Into<String>,
    question: PreferenceQuestion,
) -> CanonicalReply {
    CanonicalReply::choice(
        format!("{}\n\n{}", intro.into(), question.prompt),
        preference_choices(question.options),
    )
}

fn preference_choices(options: Vec<PreferenceOption>) -> Vec<Choice> {
    options
        .into_iter()
        .map(|option| Choice {
            id: option.id,
            label: option.label,
        })
        .collect()
}

fn login_prompt(env: &Env) -> CanonicalReply {
    CanonicalReply::web_app(
        "Tap Connect Checkers to sign in securely. Once you're connected, I'll continue with your request.",
        "Connect Checkers",
        telegram_login_url(env),
    )
}

fn is_start_command(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "/start" | "start"
    )
}

fn is_setup_only_input(input: &str) -> bool {
    let normalized = input
        .trim()
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "hi" | "hello" | "hey" | "howzit" | "yo" | "start"
    ) || is_start_command(input)
}

fn should_require_store_login(input: &str) -> bool {
    !is_setup_only_input(input)
}

async fn handle_selected_option(
    env: &Env,
    key: &str,
    session: Option<String>,
    option_id: &str,
) -> anyhow::Result<CanonicalReply> {
    let Some(cookies) = session else {
        return Ok(CanonicalReply::text(
            "I need you to log in first. Type \"login\" to connect your Checkers Sixty60 account.",
        ));
    };

    let mut client = Sixty60Client::with_session(cookies);
    worker::console_log!("selected add_to_cart start: {}", product_log_ref(option_id));
    match client.add_to_cart(option_id, 1).await {
        Ok(()) => {
            worker::console_log!("selected add_to_cart ok: {}", product_log_ref(option_id));
            save_session(env, key, client.session()).await?;
            Ok(CanonicalReply::text("Added it to your cart."))
        }
        Err(StoreError::Http(401 | 403, _)) => {
            worker::console_warn!(
                "selected add_to_cart auth failed: {}",
                product_log_ref(option_id)
            );
            clear_session(env, key).await?;
            Ok(CanonicalReply::text(
                "Your Checkers session expired. Type \"login\" to reconnect.",
            ))
        }
        Err(e) => {
            worker::console_warn!("selected add_to_cart failed: {e:?}");
            save_session(env, key, client.session()).await?;
            Ok(CanonicalReply::text(
                "I found that item, but Checkers didn't accept the cart update. Please tap it once more.",
            ))
        }
    }
}

fn product_log_ref(product_id: &str) -> String {
    let prefix: String = product_id.chars().take(12).collect();
    format!("id_prefix={prefix} id_len={}", product_id.len())
}

fn telegram_login_url(env: &Env) -> String {
    let base = env
        .var("PUBLIC_BASE_URL")
        .map(|v| v.to_string())
        .unwrap_or_else(|_| "https://shop-pilot-api.imngonii.workers.dev".to_string());
    format!("{}/telegram/login", base.trim_end_matches('/'))
}

// --- login state machine (phone → OTP → profile checks) ---------------------

#[derive(Default, Serialize, Deserialize)]
struct LoginState {
    stage: String, // "number" | "otp" | "consent" | "dob" | "id"
    mobile: String,
    otp_ref: String,
    cookies: String,
    #[serde(default)]
    scheme: Option<Value>,
    #[serde(default)]
    user_uuid: Option<String>,
    #[serde(default)]
    user_uid: Option<String>,
    #[serde(default)]
    user_exists_in_ciam: bool,
    #[serde(default)]
    has_visited: bool,
    #[serde(default)]
    is_migrated_user: bool,
}

#[derive(Serialize)]
pub struct WebLoginResponse {
    pub ok: bool,
    pub step: &'static str,
    pub message: String,
}

impl WebLoginResponse {
    fn next(step: &'static str, message: impl Into<String>) -> Self {
        Self {
            ok: true,
            step,
            message: message.into(),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            step: "error",
            message: message.into(),
        }
    }
}

pub async fn web_login_start(
    env: &Env,
    telegram_user_id: &str,
    phone: &str,
) -> anyhow::Result<WebLoginResponse> {
    let key = telegram_user_key(telegram_user_id);
    let number = normalize_msisdn(phone);
    let mut client = Sixty60Client::new();
    match client.verify_cell(&number).await {
        Ok(true) => {}
        Ok(false) => {
            clear_login_state(env, &key).await?;
            return Ok(WebLoginResponse::error(
                "I couldn't find a Checkers Sixty60 account for that number.",
            ));
        }
        Err(StoreError::Http(400, _)) => {
            clear_login_state(env, &key).await?;
            return Ok(WebLoginResponse::error(
                "That does not look like a valid Checkers Sixty60 mobile number.",
            ));
        }
        Err(e) => {
            clear_login_state(env, &key).await?;
            worker::console_warn!("web verify_cell failed: {e:?}");
            return Ok(WebLoginResponse::error(
                "I couldn't check that number just now.",
            ));
        }
    }

    match client.request_otp(&number).await {
        Ok(otp_ref) => {
            save_login_state(
                env,
                &key,
                &LoginState {
                    stage: "otp".into(),
                    mobile: number,
                    otp_ref,
                    cookies: client.session().to_string(),
                    scheme: None,
                    ..Default::default()
                },
            )
            .await?;
            Ok(WebLoginResponse::next(
                "otp",
                "I sent an OTP to your phone.",
            ))
        }
        Err(StoreError::Http(400, _)) => {
            clear_login_state(env, &key).await?;
            Ok(WebLoginResponse::error(
                "That does not look like a valid Checkers Sixty60 mobile number.",
            ))
        }
        Err(e) => {
            clear_login_state(env, &key).await?;
            worker::console_warn!("web request_otp failed: {e:?}");
            Ok(WebLoginResponse::error(
                "I couldn't start the login just now.",
            ))
        }
    }
}

pub async fn web_login_otp(
    env: &Env,
    telegram_user_id: &str,
    otp: &str,
) -> anyhow::Result<WebLoginResponse> {
    let key = telegram_user_key(telegram_user_id);
    let Some(mut st) = load_login_state(env, &key).await? else {
        return Ok(WebLoginResponse::error(
            "Start with your mobile number first.",
        ));
    };
    if st.stage != "otp" {
        return Ok(WebLoginResponse::error(
            "That login step is no longer active.",
        ));
    }

    let mut client = Sixty60Client::with_session(st.cookies.clone());
    match client.verify_otp(&st.mobile, otp.trim(), &st.otp_ref).await {
        Ok(()) => {}
        Err(e) => {
            worker::console_warn!("web verify_otp failed: {e:?}");
            return Ok(WebLoginResponse::error(
                "That OTP did not work. Try it again.",
            ));
        }
    }

    let prefetch = match client.prefetch_user_profile().await {
        Ok(prefetch) => prefetch,
        Err(e) => {
            worker::console_warn!("web prefetch_user_profile failed: {e:?}");
            return Ok(WebLoginResponse::error(
                "That code worked, but I couldn't finish the Checkers profile check.",
            ));
        }
    };
    st.cookies = client.session().to_string();
    st.scheme = Some(prefetch.scheme.clone());
    st.user_exists_in_ciam = prefetch.user_exists_in_ciam;
    st.has_visited = prefetch.has_visited;
    st.is_migrated_user = prefetch.is_migrated_user;

    if !prefetch.has_user_granted_consents {
        let identity = match client.decrypt_scheme_identity(&prefetch.scheme).await {
            Ok(identity) => identity,
            Err(e) => {
                worker::console_warn!("web decrypt_scheme_identity failed: {e:?}");
                return Ok(WebLoginResponse::error(
                    "That code worked, but I couldn't load the Checkers consent step.",
                ));
            }
        };
        st.user_uuid = identity.uuid;
        st.user_uid = identity.uid;
        st.cookies = client.session().to_string();
        st.stage = "consent".into();
        save_login_state(env, &key, &st).await?;
        return Ok(WebLoginResponse::next(
            "consent",
            "Checkers needs you to accept their Sixty60 terms before login can continue.",
        ));
    }

    continue_after_profile_gate_web(env, &key, client, st).await
}

pub async fn web_login_accept_consent(
    env: &Env,
    telegram_user_id: &str,
) -> anyhow::Result<WebLoginResponse> {
    let key = telegram_user_key(telegram_user_id);
    let Some(mut st) = load_login_state(env, &key).await? else {
        return Ok(WebLoginResponse::error(
            "Start with your mobile number first.",
        ));
    };
    if st.stage != "consent" {
        return Ok(WebLoginResponse::error(
            "There is no consent step to accept.",
        ));
    }
    let Some(uuid) = st.user_uuid.clone() else {
        clear_login_state(env, &key).await?;
        return Ok(WebLoginResponse::error(
            "I lost the Checkers consent details for this login.",
        ));
    };

    let mut client = Sixty60Client::with_session(st.cookies.clone());
    match client.accept_required_consents(&uuid).await {
        Ok(()) => {
            st.cookies = client.session().to_string();
            continue_after_profile_gate_web(env, &key, client, st).await
        }
        Err(e) => {
            worker::console_warn!("web accept_required_consents failed: {e:?}");
            Ok(WebLoginResponse::error(
                "I couldn't save that consent with Checkers just now.",
            ))
        }
    }
}

pub async fn web_login_dob(
    env: &Env,
    telegram_user_id: &str,
    date_of_birth: &str,
) -> anyhow::Result<WebLoginResponse> {
    let key = telegram_user_key(telegram_user_id);
    let Some(st) = load_login_state(env, &key).await? else {
        return Ok(WebLoginResponse::error(
            "Start with your mobile number first.",
        ));
    };
    if st.stage != "dob" {
        return Ok(WebLoginResponse::error(
            "There is no date-of-birth step active.",
        ));
    }
    let Some(dob) = normalize_date_of_birth(date_of_birth.trim()) else {
        return Ok(WebLoginResponse::error("Use DD/MM/YYYY for date of birth."));
    };
    let Some(scheme) = st.scheme.clone() else {
        clear_login_state(env, &key).await?;
        return Ok(WebLoginResponse::error(
            "I lost the Checkers profile check for this login.",
        ));
    };

    let mut client = Sixty60Client::with_session(st.cookies.clone());
    match client.verify_date_of_birth(&dob, &scheme).await {
        Ok(()) => complete_login_web(env, &key, client).await,
        Err(e) => {
            worker::console_warn!("web verify_dob failed: {e:?}");
            Ok(WebLoginResponse::error(
                "I couldn't verify that date of birth. Check it and try again.",
            ))
        }
    }
}

pub async fn web_login_identity(
    env: &Env,
    telegram_user_id: &str,
    identity: &str,
) -> anyhow::Result<WebLoginResponse> {
    let key = telegram_user_key(telegram_user_id);
    let Some(st) = load_login_state(env, &key).await? else {
        return Ok(WebLoginResponse::error(
            "Start with your mobile number first.",
        ));
    };
    if st.stage != "id" {
        return Ok(WebLoginResponse::error("There is no identity step active."));
    }
    let Some(document) = parse_identity_document(identity.trim()) else {
        return Ok(WebLoginResponse::error(
            "Send your SA ID number, or passport details like: passport AB123456 21/05/1990.",
        ));
    };

    let mut client = Sixty60Client::with_session(st.cookies.clone());
    if let Some(uid) = st.user_uid.clone() {
        match client
            .validate_id_or_passport_available(document.number(), &uid)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return Ok(WebLoginResponse::error(
                    "Checkers says that ID or passport is already linked to another profile.",
                ));
            }
            Err(e) => {
                worker::console_warn!("web validate_id_or_passport_available failed: {e:?}");
            }
        }
    }

    match client
        .update_customer_id_or_passport(document.payload())
        .await
    {
        Ok(()) => complete_login_web(env, &key, client).await,
        Err(e) => {
            worker::console_warn!("web update_customer_id_or_passport failed: {e:?}");
            Ok(WebLoginResponse::error(
                "Checkers couldn't verify that ID or passport. Check it and try again.",
            ))
        }
    }
}

pub async fn web_login_cancel(env: &Env, telegram_user_id: &str) -> anyhow::Result<()> {
    let key = telegram_user_key(telegram_user_id);
    clear_login_state(env, &key).await?;
    clear_pending_login_input(env, &key).await
}

pub async fn post_web_login_reply(
    env: &Env,
    telegram_user_id: &str,
) -> anyhow::Result<CanonicalReply> {
    let key = telegram_user_key(telegram_user_id);
    post_login_reply(env, &key).await
}

fn telegram_user_key(telegram_user_id: &str) -> String {
    format!("telegram:{telegram_user_id}")
}

/// Returns `Some(reply)` if the message was consumed by the login flow, else
/// `None` (let the agent handle it).
async fn handle_login(env: &Env, key: &str, input: &str) -> anyhow::Result<Option<CanonicalReply>> {
    let trimmed = input.trim();
    let state = load_login_state(env, key).await?;

    // Not currently logging in: only start on an explicit command.
    let Some(mut st) = state else {
        if trimmed.eq_ignore_ascii_case("login") || trimmed.eq_ignore_ascii_case("/login") {
            save_login_state(
                env,
                key,
                &LoginState {
                    stage: "number".into(),
                    ..Default::default()
                },
            )
            .await?;
            return Ok(Some(CanonicalReply::web_app(
                "Tap Connect Checkers to sign in securely. If the button does not open, you can still reply with your mobile number here.",
                "Connect Checkers",
                telegram_login_url(env),
            )));
        }
        return Ok(None);
    };

    if trimmed.eq_ignore_ascii_case("cancel") {
        clear_login_state(env, key).await?;
        clear_pending_login_input(env, key).await?;
        return Ok(Some(CanonicalReply::text("No problem — login cancelled.")));
    }

    match st.stage.as_str() {
        "number" => {
            let number = normalize_msisdn(trimmed);
            let mut client = Sixty60Client::new();
            match client.verify_cell(&number).await {
                Ok(true) => {}
                Ok(false) => {
                    clear_login_state(env, key).await?;
                    return Ok(Some(CanonicalReply::text(
                        "I couldn't find a Checkers Sixty60 account for that number. Type \"login\" to try another number.",
                    )));
                }
                Err(StoreError::Http(400, _)) => {
                    clear_login_state(env, key).await?;
                    return Ok(Some(CanonicalReply::text(
                        "That doesn't look like a valid Checkers Sixty60 mobile number. Type \"login\" to try again.",
                    )));
                }
                Err(e) => {
                    clear_login_state(env, key).await?;
                    worker::console_warn!("verify_cell failed: {e:?}");
                    return Ok(Some(CanonicalReply::text(
                        "I couldn't check that number just now. Type \"login\" to try again.",
                    )));
                }
            }
            match client.request_otp(&number).await {
                Ok(otp_ref) => {
                    let next = LoginState {
                        stage: "otp".into(),
                        mobile: number,
                        otp_ref,
                        cookies: client.session().to_string(),
                        scheme: None,
                        ..Default::default()
                    };
                    save_login_state(env, key, &next).await?;
                    Ok(Some(CanonicalReply::text(
                        "I've sent an OTP to your phone. What's the code?",
                    )))
                }
                Err(StoreError::Http(400, _)) => {
                    clear_login_state(env, key).await?;
                    Ok(Some(CanonicalReply::text(
                        "That doesn't look like a valid Checkers Sixty60 mobile number. Type \"login\" to try again.",
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
                    let prefetch = match client.prefetch_user_profile().await {
                        Ok(prefetch) => prefetch,
                        Err(e) => {
                            worker::console_warn!("prefetch_user_profile failed: {e:?}");
                            return Ok(Some(CanonicalReply::text(
                                "That code worked, but I couldn't finish the Checkers profile check. Type \"cancel\" and try login again.",
                            )));
                        }
                    };
                    st.cookies = client.session().to_string();
                    st.scheme = Some(prefetch.scheme.clone());
                    st.user_exists_in_ciam = prefetch.user_exists_in_ciam;
                    st.has_visited = prefetch.has_visited;
                    st.is_migrated_user = prefetch.is_migrated_user;

                    if !prefetch.has_user_granted_consents {
                        let identity = match client.decrypt_scheme_identity(&prefetch.scheme).await
                        {
                            Ok(identity) => identity,
                            Err(e) => {
                                worker::console_warn!("decrypt_scheme_identity failed: {e:?}");
                                return Ok(Some(CanonicalReply::text(
                                    "That code worked, but I couldn't load the Checkers consent step. Type \"cancel\" and try login again.",
                                )));
                            }
                        };
                        st.user_uuid = identity.uuid;
                        st.user_uid = identity.uid;
                        st.cookies = client.session().to_string();
                        st.stage = "consent".into();
                        save_login_state(env, key, &st).await?;
                        return Ok(Some(CanonicalReply::text(
                            "Checkers needs you to accept their Sixty60 terms before this login can continue. Reply \"accept\" to accept and continue, or \"cancel\".",
                        )));
                    }

                    continue_after_profile_gate(env, key, client, st)
                        .await
                        .map(Some)
                }
                Err(e) => {
                    worker::console_warn!("verify_otp failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "That code didn't work. Try the code again, or type \"cancel\".",
                    )))
                }
            }
        }
        "consent" => {
            if !matches!(
                trimmed.to_ascii_lowercase().as_str(),
                "accept" | "accepted" | "agree" | "yes" | "y"
            ) {
                return Ok(Some(CanonicalReply::text(
                    "Reply \"accept\" to accept Checkers' Sixty60 terms and continue, or type \"cancel\".",
                )));
            }
            let Some(uuid) = st.user_uuid.clone() else {
                clear_login_state(env, key).await?;
                return Ok(Some(CanonicalReply::text(
                    "I lost the Checkers consent details for this login. Type \"login\" to start again.",
                )));
            };
            let mut client = Sixty60Client::with_session(st.cookies.clone());
            match client.accept_required_consents(&uuid).await {
                Ok(()) => {
                    st.cookies = client.session().to_string();
                    continue_after_profile_gate(env, key, client, st)
                        .await
                        .map(Some)
                }
                Err(e) => {
                    worker::console_warn!("accept_required_consents failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "I couldn't save that consent with Checkers just now. Reply \"accept\" to try again, or type \"cancel\".",
                    )))
                }
            }
        }
        "dob" => {
            let Some(dob) = normalize_date_of_birth(trimmed) else {
                return Ok(Some(CanonicalReply::text(
                    "Send your date of birth as DD/MM/YYYY, or type \"cancel\".",
                )));
            };
            let Some(scheme) = st.scheme.clone() else {
                clear_login_state(env, key).await?;
                return Ok(Some(CanonicalReply::text(
                    "I lost the Checkers profile check for this login. Type \"login\" to start again.",
                )));
            };
            let mut client = Sixty60Client::with_session(st.cookies.clone());
            match client.verify_date_of_birth(&dob, &scheme).await {
                Ok(()) => complete_login(env, key, client).await.map(Some),
                Err(e) => {
                    worker::console_warn!("verify_dob failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "I couldn't verify that. Send your date of birth as DD/MM/YYYY, or type \"cancel\".",
                    )))
                }
            }
        }
        "id" => {
            let Some(document) = parse_identity_document(trimmed) else {
                return Ok(Some(CanonicalReply::text(
                    "Send your SA ID number, or send passport details like: passport AB123456 21/05/1990. You can type \"cancel\" to stop.",
                )));
            };
            let mut client = Sixty60Client::with_session(st.cookies.clone());
            if let Some(uid) = st.user_uid.clone() {
                match client
                    .validate_id_or_passport_available(document.number(), &uid)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        return Ok(Some(CanonicalReply::text(
                            "Checkers says that ID or passport is already linked to another profile. Please check it and send it again, or type \"cancel\".",
                        )));
                    }
                    Err(e) => {
                        worker::console_warn!("validate_id_or_passport_available failed: {e:?}");
                    }
                }
            }
            match client
                .update_customer_id_or_passport(document.payload())
                .await
            {
                Ok(()) => complete_login(env, key, client).await.map(Some),
                Err(e) => {
                    worker::console_warn!("update_customer_id_or_passport failed: {e:?}");
                    Ok(Some(CanonicalReply::text(
                        "Checkers couldn't verify that ID or passport. Please check it and send it again, or type \"cancel\".",
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

async fn continue_after_profile_gate(
    env: &Env,
    key: &str,
    client: Sixty60Client,
    mut st: LoginState,
) -> anyhow::Result<CanonicalReply> {
    if st.user_exists_in_ciam && st.has_visited {
        return complete_login(env, key, client).await;
    }

    st.cookies = client.session().to_string();
    if st.is_migrated_user {
        st.stage = "id".into();
        save_login_state(env, key, &st).await?;
        return Ok(CanonicalReply::text(
            "Checkers needs one more account check. Send your SA ID number, or send passport details like: passport AB123456 21/05/1990.",
        ));
    }

    st.stage = "dob".into();
    save_login_state(env, key, &st).await?;
    Ok(CanonicalReply::text(
        "Great. One last step: what's your date of birth? Send it as DD/MM/YYYY.",
    ))
}

async fn complete_login(
    env: &Env,
    key: &str,
    mut client: Sixty60Client,
) -> anyhow::Result<CanonicalReply> {
    match client.bootstrap_login_context().await {
        Ok(()) => {
            save_session(env, key, client.session()).await?;
            clear_login_state(env, key).await?;
            post_login_reply(env, key).await
        }
        Err(e) => {
            worker::console_warn!("bootstrap_login_context failed: {e:?}");
            Ok(CanonicalReply::text(
                "You're signed in, but I couldn't load your Checkers delivery context yet. Type \"cancel\" and try login again.",
            ))
        }
    }
}

async fn continue_after_profile_gate_web(
    env: &Env,
    key: &str,
    client: Sixty60Client,
    mut st: LoginState,
) -> anyhow::Result<WebLoginResponse> {
    if st.user_exists_in_ciam && st.has_visited {
        return complete_login_web(env, key, client).await;
    }

    st.cookies = client.session().to_string();
    if st.is_migrated_user {
        st.stage = "id".into();
        save_login_state(env, key, &st).await?;
        return Ok(WebLoginResponse::next(
            "identity",
            "Checkers needs one more account check.",
        ));
    }

    st.stage = "dob".into();
    save_login_state(env, key, &st).await?;
    Ok(WebLoginResponse::next(
        "dob",
        "One last step: enter your date of birth.",
    ))
}

async fn complete_login_web(
    env: &Env,
    key: &str,
    mut client: Sixty60Client,
) -> anyhow::Result<WebLoginResponse> {
    match client.bootstrap_login_context().await {
        Ok(()) => {
            save_session(env, key, client.session()).await?;
            clear_login_state(env, key).await?;
            Ok(WebLoginResponse::next(
                "complete",
                "You're connected to Checkers Sixty60.",
            ))
        }
        Err(e) => {
            worker::console_warn!("web bootstrap_login_context failed: {e:?}");
            Ok(WebLoginResponse::error(
                "You're signed in, but I couldn't load your Checkers delivery context yet.",
            ))
        }
    }
}

async fn post_login_reply(env: &Env, key: &str) -> anyhow::Result<CanonicalReply> {
    let Some(pending) = load_pending_login_input(env, key).await? else {
        return Ok(CanonicalReply::text(
            "You're connected to Checkers Sixty60. Tell me what you'd like to buy.",
        ));
    };
    clear_pending_login_input(env, key).await?;

    let prefs = load_preferences(env, key).await?;
    if let Some(question) = prefs.next_question() {
        save_pending_preference_input(env, key, &pending).await?;
        return Ok(preference_question_reply(question));
    }
    run_agent(env, key, pending, prefs).await
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

async fn clear_session(env: &Env, key: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.delete(&format!("sess:{key}")).await.map_err(werr)?;
    Ok(())
}

async fn sync_session(
    env: &Env,
    key: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> anyhow::Result<()> {
    match (before, after) {
        (_, Some(next)) if before != Some(next) => save_session(env, key, next).await,
        (Some(_), None) => clear_session(env, key).await,
        _ => Ok(()),
    }
}

// --- preferences (KV) --------------------------------------------------------

async fn load_preferences(env: &Env, key: &str) -> anyhow::Result<UserPreferences> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv.get(&format!("pref:{key}")).text().await.map_err(werr)?;
    let Some(raw) = raw else {
        return Ok(UserPreferences::default());
    };
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

async fn save_preferences(
    env: &Env,
    key: &str,
    preferences: &UserPreferences,
) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(&format!("pref:{key}"), serde_json::to_string(preferences)?)
        .map_err(werr)?
        .execute()
        .await
        .map_err(werr)?;
    Ok(())
}

async fn clear_preferences(env: &Env, key: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.delete(&format!("pref:{key}")).await.map_err(werr)?;
    Ok(())
}

async fn load_pending_preference_input(env: &Env, key: &str) -> anyhow::Result<Option<String>> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv
        .get(&format!("pref_pending:{key}"))
        .text()
        .await
        .map_err(werr)?;
    Ok(raw.filter(|s| !s.trim().is_empty()))
}

async fn save_pending_preference_input(env: &Env, key: &str, input: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(&format!("pref_pending:{key}"), input)
        .map_err(werr)?
        .execute()
        .await
        .map_err(werr)?;
    Ok(())
}

async fn clear_pending_preference_input(env: &Env, key: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.delete(&format!("pref_pending:{key}"))
        .await
        .map_err(werr)?;
    Ok(())
}

async fn load_pending_login_input(env: &Env, key: &str) -> anyhow::Result<Option<String>> {
    let kv = env.kv("CACHE").map_err(werr)?;
    let raw = kv
        .get(&format!("login_pending:{key}"))
        .text()
        .await
        .map_err(werr)?;
    Ok(raw.filter(|s| !s.trim().is_empty()))
}

async fn save_pending_login_input(env: &Env, key: &str, input: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.put(&format!("login_pending:{key}"), input)
        .map_err(werr)?
        .execute()
        .await
        .map_err(werr)?;
    Ok(())
}

async fn clear_pending_login_input(env: &Env, key: &str) -> anyhow::Result<()> {
    let kv = env.kv("CACHE").map_err(werr)?;
    kv.delete(&format!("login_pending:{key}"))
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

#[derive(Debug, Clone)]
struct CartWrite {
    name: String,
    qty: u32,
    price_cents: u64,
}

#[derive(Debug, Clone)]
struct CartWriteFailure {
    name: String,
}

pub struct ShopPilotAgent {
    /// Sixty60 session cookies, if the user is connected. `None` → mock tools.
    session: Arc<Mutex<Option<String>>>,
    preferences: UserPreferences,
    /// Mock shopping is only valid for users who began this turn logged out.
    /// If a real session is rejected mid-turn, tools must surface re-auth
    /// instead of silently switching to a fake cart.
    allow_mock: bool,
    cart: Arc<Mutex<Vec<CartLine>>>,
    successful_adds: Arc<Mutex<Vec<CartWrite>>>,
    failed_adds: Arc<Mutex<Vec<CartWriteFailure>>>,
}

impl ShopPilotAgent {
    pub fn new(session: Option<String>, preferences: UserPreferences) -> Self {
        let allow_mock = session.is_none();
        Self {
            session: Arc::new(Mutex::new(session)),
            preferences,
            allow_mock,
            cart: Arc::new(Mutex::new(Vec::new())),
            successful_adds: Arc::new(Mutex::new(Vec::new())),
            failed_adds: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn session_snapshot(&self) -> Option<String> {
        self.session.lock().unwrap().clone()
    }

    fn successful_adds_snapshot(&self) -> Vec<CartWrite> {
        self.successful_adds.lock().unwrap().clone()
    }

    fn failed_adds_snapshot(&self) -> Vec<CartWriteFailure> {
        self.failed_adds.lock().unwrap().clone()
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
        format!(
            "{}\n\n{}",
            INSTRUCTIONS,
            self.preferences.agent_instructions()
        )
    }

    fn model(&self) -> String {
        MODEL.to_string()
    }

    fn tools(&self) -> ToolRegistry<()> {
        let mut r = ToolRegistry::new();

        // search_products — real store when connected, else mock.
        let session = self.session.clone();
        let allow_mock = self.allow_mock;
        r.register(JsonTool::new(
            "search_products",
            "Search the Checkers Sixty60 catalogue for a grocery item. Returns candidate \
             products with sku_id, name, price_cents and stock.",
            move |_c: (), a: SearchArgs| {
                let session = session.clone();
                SendWrapper::new(async move {
                    let cookies = session.lock().unwrap().clone();
                    match cookies {
                        Some(cookies) => {
                            let mut client = Sixty60Client::with_session(cookies);
                            match client.search(&a.query).await {
                                Ok(products) => {
                                    *session.lock().unwrap() = Some(client.session().to_string());
                                    Ok(json!({
                                        "products": products.iter().map(|p| json!({
                                            "sku_id": p.product_id,
                                            "name": p.name,
                                            "brand": p.brand,
                                            "price_cents": p.price_cents,
                                            "in_stock": p.in_stock,
                                        })).collect::<Vec<_>>()
                                    }))
                                }
                                Err(StoreError::Http(401 | 403, _)) => {
                                    *session.lock().unwrap() = None;
                                    Ok(json!({
                                        "needs_reauth": true,
                                        "error": "Checkers rejected the saved session.",
                                        "products": []
                                    }))
                                }
                                Err(e) => Ok(json!({
                                    "error": format!("{e:?}"),
                                    "products": []
                                })),
                            }
                        }
                        None if allow_mock => Ok(mock_search(&a.query)),
                        None => Ok(json!({
                            "needs_reauth": true,
                            "error": "Checkers rejected the saved session.",
                            "products": []
                        })),
                    }
                })
            },
        ));

        // add_to_cart — real store when connected, else local mock cart.
        let session = self.session.clone();
        let allow_mock = self.allow_mock;
        let cart = self.cart.clone();
        let successful_adds = self.successful_adds.clone();
        let failed_adds = self.failed_adds.clone();
        r.register(JsonTool::new(
            "add_to_cart",
            "Add a specific product (by sku_id) to the user's cart.",
            move |_c: (), a: AddArgs| {
                let session = session.clone();
                let cart = cart.clone();
                let successful_adds = successful_adds.clone();
                let failed_adds = failed_adds.clone();
                SendWrapper::new(async move {
                    let cookies = session.lock().unwrap().clone();
                    if let Some(cookies) = cookies {
                        let mut client = Sixty60Client::with_session(cookies);
                        return match client.add_to_cart(&a.sku_id, a.qty.max(1)).await {
                            Ok(()) => {
                                *session.lock().unwrap() = Some(client.session().to_string());
                                successful_adds.lock().unwrap().push(CartWrite {
                                    name: a.name,
                                    qty: a.qty.max(1),
                                    price_cents: a.price_cents,
                                });
                                Ok(json!({ "ok": true, "verified_in_cart": true }))
                            }
                            Err(StoreError::Http(401 | 403, _)) => {
                                *session.lock().unwrap() = None;
                                failed_adds
                                    .lock()
                                    .unwrap()
                                    .push(CartWriteFailure { name: a.name });
                                Ok(json!({
                                    "ok": false,
                                    "needs_reauth": true,
                                    "error": "Checkers rejected the saved session."
                                }))
                            }
                            Err(e) => {
                                let error = format!("{e:?}");
                                failed_adds
                                    .lock()
                                    .unwrap()
                                    .push(CartWriteFailure { name: a.name });
                                Ok(json!({
                                    "ok": false,
                                    "cart_update_failed": true,
                                    "needs_reauth": false,
                                    "error": error
                                }))
                            }
                        };
                    }
                    if !allow_mock {
                        failed_adds
                            .lock()
                            .unwrap()
                            .push(CartWriteFailure { name: a.name });
                        return Ok(json!({
                            "ok": false,
                            "needs_reauth": true,
                            "error": "Checkers rejected the saved session."
                        }));
                    }
                    let size = {
                        let mut c = cart.lock().unwrap();
                        c.push(CartLine {
                            sku_id: a.sku_id,
                            name: a.name.clone(),
                            qty: a.qty.max(1),
                            price_cents: a.price_cents,
                        });
                        c.len()
                    };
                    successful_adds.lock().unwrap().push(CartWrite {
                        name: a.name,
                        qty: a.qty.max(1),
                        price_cents: a.price_cents,
                    });
                    Ok(json!({ "ok": true, "cart_size": size }))
                })
            },
        ));

        // view_cart — real store when connected, else local mock cart.
        let session = self.session.clone();
        let allow_mock = self.allow_mock;
        let cart = self.cart.clone();
        r.register(JsonTool::new(
            "view_cart",
            "View the current cart contents and total.",
            move |_c: (), _a: NoArgs| {
                let session = session.clone();
                let cart = cart.clone();
                SendWrapper::new(async move {
                    let cookies = session.lock().unwrap().clone();
                    if let Some(cookies) = cookies {
                        let mut client = Sixty60Client::with_session(cookies);
                        return match client.fetch_cart().await {
                            Ok(v) => {
                                *session.lock().unwrap() = Some(client.session().to_string());
                                Ok(v)
                            }
                            Err(StoreError::Http(401 | 403, _)) => {
                                *session.lock().unwrap() = None;
                                Ok(json!({
                                    "needs_reauth": true,
                                    "error": "Checkers rejected the saved session."
                                }))
                            }
                            Err(e) => Ok(json!({
                                "cart_update_failed": true,
                                "needs_reauth": false,
                                "error": format!("{e:?}")
                            })),
                        };
                    }
                    if !allow_mock {
                        return Ok(json!({
                            "needs_reauth": true,
                            "error": "Checkers rejected the saved session."
                        }));
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
                let connected = session.lock().unwrap().is_some();
                let cart = cart.clone();
                async move {
                    let total: u64 = cart
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|l| l.price_cents * l.qty as u64)
                        .sum();
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
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
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
        let body: HttpByteStream = Box::pin(futures_util::stream::once(async move {
            Ok::<_, anyhow::Error>(bytes)
        }));
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
        let body_str = String::from_utf8(request.body)
            .map_err(|e| anyhow::anyhow!("request body utf8: {e}"))?;
        init.with_body(Some(JsValue::from_str(&body_str)));
    }

    let req = WorkerRequest::new_with_init(&request.url, &init).map_err(werr)?;
    let mut resp = Fetch::Request(req).send().await.map_err(werr)?;
    let status = resp.status_code();
    let body = resp.bytes().await.map_err(werr)?;
    Ok(HttpResponse { status, body })
}
