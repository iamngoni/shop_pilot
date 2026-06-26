# Shop Pilot Connector (browser extension)

Syncs your authenticated Checkers Sixty60 session to your Shop Pilot bot so it can
build your cart as you. The session is read **on your machine** (the extension's
cookies API can read httpOnly cookies that page scripts can't) and sent only to
your own bot's Worker, against a one-time code.

## How a user connects
1. In the chat (Telegram), send **`connect`** → the bot replies with a 6-digit code.
2. While signed in to `checkers.co.za`, open this extension and enter the code.
3. Done — the bot now acts on your Checkers session. Re-run if it expires.

## Load it (developer / unsigned)

### Safari
1. Safari → Settings → **Advanced** → enable "Show features for web developers".
2. Settings → **Developer** → tick **"Allow unsigned extensions"**.
3. Settings → **Developer** → **"Add Temporary Extension…"** → select this `extension/` folder's `manifest.json`.
   (Safari Web Extensions are MV3; for permanent install, wrap with `xcrun safari-web-extension-converter extension/`.)

### Chrome / Brave / Edge / Arc
1. `chrome://extensions` → enable **Developer mode**.
2. **Load unpacked** → select this `extension/` folder.
   (You must be signed in to checkers.co.za *in that browser*.)

## Files
- `manifest.json` — MV3, `cookies` permission for `*.checkers.co.za` / `*.sixty60.co.za` + the Worker host.
- `popup.html` / `popup.js` — enter code → read cookies → `POST /session {code, cookies}`.
