//! Workers entrypoint (wasm-only). Receives channel webhooks, routes each into
//! the engine, and sends the engine's reply back through the originating
//! channel adapter. The full inbound → engine → outbound loop, end to end.

use worker::{Context, Env, Request, Response, Result, Router, event};

use crate::{ai, telegram};

const TELEGRAM_BOT_TOKEN: &str = "TELEGRAM_BOT_TOKEN";

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    Router::new()
        .get("/health", |_, _| Response::ok("ok"))
        .post_async("/webhook/telegram", handle_telegram_webhook)
        // Browser-extension session ingest: {code, cookies}. CORS-open so the
        // extension popup (different origin) can POST.
        .options("/session", |_, _| {
            let headers = worker::Headers::new();
            let _ = headers.set("access-control-allow-origin", "*");
            let _ = headers.set("access-control-allow-methods", "POST, OPTIONS");
            let _ = headers.set("access-control-allow-headers", "content-type");
            Ok(Response::empty()?.with_headers(headers))
        })
        .post_async("/session", |mut req, ctx| async move {
            let raw = req.text().await?;
            let headers = worker::Headers::new();
            let _ = headers.set("access-control-allow-origin", "*");
            match ai::ingest_session(&ctx.env, &raw).await {
                Ok(()) => Ok(Response::ok("connected")?.with_headers(headers)),
                Err(e) => {
                    worker::console_warn!("session ingest failed: {e}");
                    Ok(Response::error(format!("{e}"), 400)?.with_headers(headers))
                }
            }
        })
        .run(req, env)
        .await
}

async fn handle_telegram_webhook(
    mut req: Request,
    ctx: worker::RouteContext<()>,
) -> Result<Response> {
    let raw = req.text().await?;

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

    // AI engine: intent in, intent out. Loads/persists conversation history and
    // runs the agent, returning a structured reply mapped to CanonicalReply.
    let reply = ai::respond(&ctx.env, &msg).await;
    // Observability during live validation — surfaces the bot's reply in tail
    // even when the Telegram send fails (e.g. synthetic test chat).
    worker::console_log!("[reply -> {}] {:?}", msg.user_id, reply);

    let token = ctx
        .secret(TELEGRAM_BOT_TOKEN)
        .map(|s| s.to_string())
        .map_err(|_| worker::Error::RustError("TELEGRAM_BOT_TOKEN not set".into()))?;

    telegram::send(&token, &msg.user_id, &reply).await?;

    Response::ok("ok")
}
