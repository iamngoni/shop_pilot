# Shop Pilot

Buy groceries by chat. Tell it what you want, it builds your cart at your store
(Checkers Sixty60 first) and hands you to checkout. Telegram is the current live
channel; the canonical protocol leaves room for WhatsApp.

Full spec lives in Inkdrop: **"Shop Pilot — Product & Architecture Spec"**.

## Architecture (one line)

`channel adapter → canonical message → engine (intent) → channel adapter renders → store adapter`

The engine never knows which channel it's talking to and never expresses
presentation — only intent. See `src/protocol.rs` for the contract.

## Layout

| File | Role |
|------|------|
| `src/protocol.rs` | Canonical message/reply types — the channel boundary contract (pure) |
| `src/telegram.rs` | Telegram adapter — parse webhook + render/send (parse & render are pure) |
| `src/ai.rs` | AI engine + deterministic Checkers login/preferences flow (wasm-only) |
| `src/login_flow.rs` | Pure helpers for Checkers mobile, DOB, and identity parsing |
| `src/preferences.rs` | Pure shopping mode and preference model |
| `src/reply.rs` | Structured AI reply envelope mapped to canonical replies |
| `src/sixty60.rs` | Checkers Sixty60 store adapter (login, search, cart) |
| `src/sixty60_contract.rs` | Pure browser-shaped Checkers request-body builders |
| `src/session.rs` | `UserSession` Durable Object — per-user state (wasm-only) |
| `src/worker_app.rs` | Workers entrypoint + router (wasm-only) |

## Dev

```bash
# Unit tests for the pure modules and browser-contract helpers:
cargo test

# Provision Cloudflare resources (same account as Rerout), then paste the
# returned ids into wrangler.jsonc:
wrangler d1 create shop-pilot-db
wrangler kv namespace create CACHE
wrangler d1 execute shop-pilot-db --file migrations/0001_init.sql

# Secrets:
wrangler secret put TELEGRAM_BOT_TOKEN     # from @BotFather
wrangler secret put ANTHROPIC_API_KEY      # AI reply generation
wrangler secret put SENTRY_DSN             # optional error capture

# Run locally / deploy (build/ is produced by worker-build):
wrangler dev
wrangler deploy

# Point Telegram at the deployed webhook:
#   https://api.telegram.org/bot<TOKEN>/setWebhook?url=https://shop-pilot-api.<subdomain>.workers.dev/webhook/telegram
#
# Telegram Mini App login is served from:
#   https://shop-pilot-api.<subdomain>.workers.dev/telegram/login
```

## Status

Telegram chat loop with an AI engine, deterministic inline-button handling, and
real Sixty60 catalogue/cart tools when the user is logged in. Logged-out users
still get mock shopping tools for demo flow.

Shopping preferences are stored per Telegram user in KV. On first shopping use,
the bot asks for mode, shopping style, pantry-basics behavior, and substitution
behavior, then resumes the original shopping request. `/mode` switches between
manual mode (show options first) and auto mode (let the AI choose and add when
confidence is high). `/preferences` shows the saved setup, and
`/preferences reset` starts it over.

The browser-extension session-capture path has been removed. Telegram login now
opens a Mini App form for phone, OTP, consent, DOB, and identity checks. The
backend still follows the proven browser sequence: verify cell, request OTP,
verify OTP, prefetch profile, optionally accept required consents, optionally
verify date of birth or identity, then hydrate delivery/store context for
catalogue and cart calls.
