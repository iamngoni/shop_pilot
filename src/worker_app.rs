//! Workers entrypoint (wasm-only). Receives channel webhooks, routes each into
//! the engine, and sends the engine's reply back through the originating
//! channel adapter. The full inbound → engine → outbound loop, end to end.

use worker::{Context, Env, Request, Response, Result, Router, event};

use crate::{engine, telegram};

const TELEGRAM_BOT_TOKEN: &str = "TELEGRAM_BOT_TOKEN";

#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    console_error_panic_hook::set_once();

    Router::new()
        .get("/health", |_, _| Response::ok("ok"))
        .post_async("/webhook/telegram", handle_telegram_webhook)
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

    // Engine: intent in, intent out. Stateless for now; the UserSession DO gets
    // wired in once conversation state is needed.
    let reply = engine::handle(&msg);

    let token = ctx
        .secret(TELEGRAM_BOT_TOKEN)
        .map(|s| s.to_string())
        .map_err(|_| worker::Error::RustError("TELEGRAM_BOT_TOKEN not set".into()))?;

    telegram::send(&token, &msg.user_id, &reply).await?;

    Response::ok("ok")
}
