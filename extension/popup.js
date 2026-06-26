// Reads the user's Checkers session cookies (incl. httpOnly — the extension
// cookies API can, unlike page JS) and posts them to the Worker against the
// one-time code shown in the chat. The session never leaves the user's machine
// except this single, user-initiated sync to their own bot's Worker.

const WORKER = "https://shop-pilot-api.imngonii.workers.dev/session";
// Cross-browser: Safari uses `browser`, Chromium uses `chrome`.
const api = typeof browser !== "undefined" ? browser : chrome;

const $ = (id) => document.getElementById(id);

async function collectCookies() {
  // Gather every cookie scoped to the Checkers / Sixty60 hosts.
  const domains = ["checkers.co.za", "www.checkers.co.za", "sixty60.co.za"];
  const seen = new Map(); // name -> value (last wins)
  for (const domain of domains) {
    const cookies = await api.cookies.getAll({ domain });
    for (const c of cookies) seen.set(c.name, c.value);
  }
  return Array.from(seen, ([name, value]) => `${name}=${value}`).join("; ");
}

$("go").addEventListener("click", async () => {
  const code = $("code").value.trim();
  const status = $("status");
  status.className = "";
  if (!/^\d{6}$/.test(code)) {
    status.textContent = "Enter the 6-digit code from your chat.";
    status.className = "err";
    return;
  }
  $("go").disabled = true;
  status.textContent = "Reading your session…";
  try {
    const cookies = await collectCookies();
    if (!cookies) throw new Error("No Checkers session found — sign in at checkers.co.za first.");
    const res = await fetch(WORKER, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code, cookies }),
    });
    if (res.ok) {
      status.textContent = "✅ Connected! Head back to your chat.";
      status.className = "ok";
    } else {
      status.textContent = "❌ " + (await res.text());
      status.className = "err";
    }
  } catch (e) {
    status.textContent = "❌ " + e.message;
    status.className = "err";
  } finally {
    $("go").disabled = false;
  }
});
