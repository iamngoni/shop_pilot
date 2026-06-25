# Shop Pilot

Buy groceries by chat. Tell it what you want, it builds your cart at your store
(Checkers Sixty60 first) and hands you to checkout — across Telegram and WhatsApp.

Full spec lives in Inkdrop: **"Shop Pilot — Product & Architecture Spec"**.

## Architecture (one line)

`channel adapter → canonical message → engine (intent) → channel adapter renders → store adapter`

The engine never knows which channel it's talking to and never expresses
presentation — only intent. See `src/protocol.rs` for the contract.

## Layout

| File | Role |
|------|------|
| `src/protocol.rs` | Canonical message/reply types — the channel boundary contract (pure) |
| `src/engine.rs` | Channel-agnostic engine (stub: deterministic, no LLM yet) |
| `src/telegram.rs` | Telegram adapter — parse webhook + render/send (parse & render are pure) |
| `src/sixty60.rs` | Sixty60 store adapter — **stub, gated on the spike** |
| `src/session.rs` | `UserSession` Durable Object — per-user state (wasm-only) |
| `src/worker_app.rs` | Workers entrypoint + router (wasm-only) |

## Dev

```bash
# Unit tests for the pure modules (protocol, engine, telegram parse/render):
cargo test

# Provision Cloudflare resources (same account as Rerout), then paste the
# returned ids into wrangler.jsonc:
wrangler d1 create shop-pilot-db
wrangler kv namespace create CACHE
wrangler d1 execute shop-pilot-db --file migrations/0001_init.sql

# Secrets:
wrangler secret put TELEGRAM_BOT_TOKEN     # from @BotFather
wrangler secret put SESSION_KEY            # for sealing store sessions

# Run locally / deploy (build/ is produced by worker-build):
wrangler dev
wrangler deploy

# Point Telegram at the deployed webhook:
#   https://api.telegram.org/bot<TOKEN>/setWebhook?url=https://shop-pilot-api.<subdomain>.workers.dev/webhook/telegram
```

## Status

Runnable Telegram chat loop with a stub engine. **Next gate:** the Sixty60
egress spike (spec §7) before any store integration is built.
