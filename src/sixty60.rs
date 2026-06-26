//! Checkers Sixty60 store client (wasm-only).
//!
//! Real implementation against the API contract reverse-engineered from the
//! authenticated web app (`www.checkers.co.za`, a Next.js BFF) — see the Inkdrop
//! spec §7a/§7b. Headless login (phone → OTP → birth-date) + search + cart, all
//! as the user via their captured session cookies.
//!
//! STATUS: correct-by-contract but **UNTESTED end-to-end** — the request/response
//! shapes are inferred from the app's JS bundle and an egress spike that proved a
//! Worker reaches the origin (no WAF block). The exact field names and cookie
//! handling are confirmed on the first real login. The live bot stays on mock
//! tools until that pass; this module is the ready-to-wire real adapter.

use serde_json::{Value, json};
use worker::wasm_bindgen::JsValue;
use worker::{Fetch, Headers, Method, Request, RequestInit};

const BASE: &str = "https://www.checkers.co.za";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                  (KHTML, like Gecko) Version/18.0 Safari/605.1.15";

#[derive(Debug)]
pub enum StoreError {
    /// Non-2xx from the store (status, body preview).
    Http(u16, String),
    /// Response wasn't the JSON shape we expected.
    Parse(String),
    /// Transport/Fetch failure.
    Network(String),
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// A purchasable product resolved from a search.
#[derive(Debug, Clone)]
pub struct Product {
    pub product_id: String,
    pub name: String,
    pub brand: Option<String>,
    pub price_cents: u64,
    pub in_stock: bool,
}

/// A Sixty60 session: the accumulated cookie header that authenticates the user.
/// Captured during login and replayed on every call. Persist it (encrypted) per
/// user so the bot can act between turns.
pub struct Sixty60Client {
    cookies: String,
}

impl Sixty60Client {
    pub fn new() -> Self {
        Self { cookies: String::new() }
    }

    /// Resume from a previously captured session cookie string.
    pub fn with_session(cookies: impl Into<String>) -> Self {
        Self { cookies: cookies.into() }
    }

    /// The current session cookie string (persist this, encrypted).
    pub fn session(&self) -> &str {
        &self.cookies
    }

    // --- login: phone → OTP → birth-date ------------------------------------

    /// Step 1: request an OTP SMS for `mobile_number`; returns the `OTPReference`.
    pub async fn request_otp(&mut self, mobile_number: &str) -> StoreResult<String> {
        let v = self
            .post("/api/login/request-mobile-otp", json!({ "mobileNumber": mobile_number }))
            .await?;
        first_str(&v, &["OTPReference", "otpReference", "reference"])
            .ok_or_else(|| StoreError::Parse(format!("no OTPReference in response: {v}")))
    }

    /// Step 2: verify the OTP the user relayed.
    pub async fn verify_otp(
        &mut self,
        mobile_number: &str,
        otp: &str,
        otp_reference: &str,
    ) -> StoreResult<()> {
        self.post(
            "/api/login/verify-otp",
            json!({
                "OTP": otp,
                "OTPReference": otp_reference,
                "mobileNumber": mobile_number,
                "isEmail": false,
            }),
        )
        .await?;
        Ok(())
    }

    /// Step 3: verify date of birth (e.g. "1990-05-21"); completes the session.
    pub async fn verify_date_of_birth(&mut self, date_of_birth: &str) -> StoreResult<()> {
        self.post(
            "/api/login/verify-date-of-birth",
            json!({ "dateOfBirth": date_of_birth }),
        )
        .await?;
        Ok(())
    }

    // --- shopping -----------------------------------------------------------

    /// Search the catalogue; returns candidate products.
    pub async fn search(&mut self, query: &str) -> StoreResult<Vec<Product>> {
        let v = self
            .post(
                "/api/v2/search/products/",
                json!({ "searchTerm": query, "pageNumber": 0, "pageSize": 10 }),
            )
            .await?;
        Ok(parse_products(&v))
    }

    /// Add a product to the user's cart.
    pub async fn add_to_cart(&mut self, product_id: &str, quantity: u32) -> StoreResult<()> {
        self.post(
            "/api/cart/update-cart",
            json!({
                "payload": { "productId": product_id, "quantity": quantity },
                "isNaiveUpdate": false,
            }),
        )
        .await?;
        Ok(())
    }

    /// Fetch the current cart (raw JSON — shape confirmed on first live run).
    pub async fn fetch_cart(&mut self) -> StoreResult<Value> {
        self.post("/api/cart/fetch-cart", json!({})).await
    }

    /// Hand-off URL: the user confirms + pays in the native app (we never custody
    /// money — see spec §9).
    pub fn checkout_url(&self) -> String {
        format!("{BASE}/cart")
    }

    // --- HTTP plumbing ------------------------------------------------------

    async fn post(&mut self, path: &str, body: Value) -> StoreResult<Value> {
        let (status, text) = self.send(path, body.to_string()).await?;
        if !(200..300).contains(&status) {
            return Err(StoreError::Http(status, text.chars().take(300).collect()));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| StoreError::Parse(e.to_string()))
    }

    async fn send(&mut self, path: &str, body: String) -> StoreResult<(u16, String)> {
        let url = format!("{BASE}{path}");
        let headers = Headers::new();
        let _ = headers.set("content-type", "application/json");
        let _ = headers.set("accept", "application/json");
        let _ = headers.set("origin", BASE);
        let _ = headers.set("referer", BASE);
        let _ = headers.set("user-agent", UA);
        if !self.cookies.is_empty() {
            let _ = headers.set("cookie", &self.cookies);
        }

        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_headers(headers)
            .with_body(Some(JsValue::from_str(&body)));

        let req = Request::new_with_init(&url, &init)
            .map_err(|e| StoreError::Network(e.to_string()))?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| StoreError::Network(e.to_string()))?;

        if let Ok(Some(set_cookie)) = resp.headers().get("set-cookie") {
            self.merge_cookies(&set_cookie);
        }
        let status = resp.status_code();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    /// Merge `Set-Cookie` values into the session jar (upsert by name). The Fetch
    /// API may comma-join multiple cookies; we split best-effort on cookie
    /// boundaries and keep the `name=value` pair, dropping attributes.
    fn merge_cookies(&mut self, set_cookie: &str) {
        for raw in set_cookie.split(", ") {
            let pair = raw.split(';').next().unwrap_or("").trim();
            let Some(name) = pair.split('=').next() else { continue };
            if name.is_empty() || !pair.contains('=') {
                continue;
            }
            let prefix = format!("{name}=");
            let kept: Vec<String> = self
                .cookies
                .split("; ")
                .filter(|c| !c.is_empty() && !c.starts_with(&prefix))
                .map(String::from)
                .collect();
            let mut next = kept;
            next.push(pair.to_string());
            self.cookies = next.join("; ");
        }
    }
}

impl Default for Sixty60Client {
    fn default() -> Self {
        Self::new()
    }
}

/// Defensively walk an arbitrary JSON response and pull out product-like objects.
/// Tolerant of the exact envelope (`{products:[…]}`, `{data:{products:[…]}}`, …)
/// since that's confirmed on the first live response.
fn parse_products(v: &Value) -> Vec<Product> {
    fn find_array<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
        match v {
            Value::Array(items) => {
                if items.iter().any(is_product) {
                    out.push(v);
                }
                for it in items {
                    find_array(it, out);
                }
            }
            Value::Object(map) => {
                for val in map.values() {
                    find_array(val, out);
                }
            }
            _ => {}
        }
    }
    let mut arrays = Vec::new();
    find_array(v, &mut arrays);
    let mut products = Vec::new();
    if let Some(arr) = arrays.into_iter().find_map(|a| a.as_array()) {
        for item in arr {
            if let Some(p) = product_from(item) {
                products.push(p);
            }
        }
    }
    products
}

fn is_product(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        o.keys()
            .any(|k| matches!(k.to_lowercase().as_str(), "productid" | "product_id" | "barcode"))
    })
}

fn product_from(v: &Value) -> Option<Product> {
    let o = v.as_object()?;
    let product_id = first_str(v, &["productId", "product_id", "id"])?;
    let name = first_str(v, &["name", "Name", "productName"]).unwrap_or_default();
    let brand = first_str(v, &["brand", "Brand"]);
    // Price may be rands (float) under price/Price/Now — normalize to cents.
    let price_cents = o
        .iter()
        .find(|(k, _)| matches!(k.to_lowercase().as_str(), "price" | "now"))
        .and_then(|(_, val)| val.as_f64())
        .map(|r| (r * 100.0).round() as u64)
        .unwrap_or(0);
    let in_stock = first_str(v, &["stockStatus", "stock"])
        .map(|s| !s.eq_ignore_ascii_case("outofstock"))
        .unwrap_or(true);
    Some(Product { product_id, name, brand, price_cents, in_stock })
}

fn first_str(v: &Value, keys: &[&str]) -> Option<String> {
    let o = v.as_object()?;
    for k in keys {
        if let Some(s) = o.get(*k).and_then(Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}
