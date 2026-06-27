//! Checkers Sixty60 store client (wasm-only).
//!
//! Real implementation against the API contract reverse-engineered from the
//! authenticated web app (`www.checkers.co.za`, a Next.js BFF) — see the Inkdrop
//! spec §7a/§7b. Headless login follows the browser flow: verify cell, request
//! OTP, verify OTP, prefetch profile, optionally verify DOB, then hydrate the
//! user's delivery/store context before search + cart calls.
//!
//! The live OTP/DOB step still requires an explicit user-approved login attempt;
//! local verification covers compile/runtime shape without sending credentials.

use serde_json::Value;
use worker::wasm_bindgen::JsValue;
use worker::{Fetch, Headers, Method, Request, RequestInit};

use crate::sixty60_contract as contract;

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

#[derive(Debug, Clone)]
pub struct LoginPrefetch {
    pub has_user_granted_consents: bool,
    pub is_migrated_user: bool,
    pub user_exists_in_ciam: bool,
    pub scheme: Value,
    pub has_visited: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoginIdentity {
    pub uuid: Option<String>,
    pub uid: Option<String>,
}

/// A purchasable product resolved from a search.
#[derive(Debug, Clone)]
pub struct Product {
    pub product_id: String,
    pub name: String,
    pub brand: Option<String>,
    pub price_cents: u64,
    pub in_stock: bool,
    pub variable_weight_options: Vec<Value>,
}

/// A Sixty60 session: the accumulated cookie header that authenticates the user.
/// Built during login and replayed on every call. Persist it per user so the bot
/// can act between turns.
pub struct Sixty60Client {
    cookies: String,
}

impl Sixty60Client {
    pub fn new() -> Self {
        Self {
            cookies: String::new(),
        }
    }

    /// Resume from a previously persisted session cookie string.
    pub fn with_session(cookies: impl Into<String>) -> Self {
        Self {
            cookies: cookies.into(),
        }
    }

    /// The current session cookie string.
    pub fn session(&self) -> &str {
        &self.cookies
    }

    // --- login: phone → OTP → profile checks --------------------------------

    /// Browser preflight: confirm the number belongs to an existing account.
    pub async fn verify_cell(&mut self, mobile_number: &str) -> StoreResult<bool> {
        let v = self
            .post(
                "/api/login/verify-cell",
                contract::verify_cell_body(mobile_number),
            )
            .await?;
        Ok(bool_at(&v, "/numberExists").unwrap_or(false))
    }

    /// Step 1: request an OTP SMS for `mobile_number`; returns the browser
    /// `response.reference` value used as `OTPReference`.
    pub async fn request_otp(&mut self, mobile_number: &str) -> StoreResult<String> {
        let v = self
            .post(
                "/api/login/request-mobile-otp",
                contract::request_mobile_otp_body(mobile_number),
            )
            .await?;
        str_at(&v, "/response/reference")
            .or_else(|| str_at(&v, "/reference"))
            .or_else(|| first_str(&v, &["OTPReference", "otpReference"]))
            .ok_or_else(|| StoreError::Parse(format!("no OTP reference in response: {v}")))
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
            contract::verify_otp_body(mobile_number, otp, otp_reference),
        )
        .await?;
        Ok(())
    }

    /// Browser prefetch after OTP verification. This returns the `scheme` object
    /// that the DOB verification endpoint requires.
    pub async fn prefetch_user_profile(&mut self) -> StoreResult<LoginPrefetch> {
        let v = self
            .post("/api/user/prefetch-user-profile", contract::empty_body())
            .await?;
        Ok(LoginPrefetch {
            has_user_granted_consents: bool_at(&v, "/hasUserGrantedConsents").unwrap_or(false),
            is_migrated_user: bool_at(&v, "/isMigratedUser").unwrap_or(false),
            user_exists_in_ciam: bool_at(&v, "/userExistsInCIAM").unwrap_or(false),
            scheme: v.get("scheme").cloned().unwrap_or(Value::Null),
            has_visited: bool_at(&v, "/hasVisited").unwrap_or(false),
        })
    }

    /// The consent modal decrypts the `scheme` value, then uses its UUID for
    /// consent writes.
    pub async fn decrypt_scheme_identity(&mut self, scheme: &Value) -> StoreResult<LoginIdentity> {
        let v = self
            .post(
                "/api/encryption/decrypt",
                contract::decrypt_scheme_body(scheme)
                    .map_err(|e| StoreError::Parse(e.to_string()))?,
            )
            .await?;
        let decrypted = match v.as_str() {
            Some(s) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.into())),
            None => v,
        };
        let identity = LoginIdentity {
            uuid: deep_str(&decrypted, "uuid").or_else(|| deep_str(scheme, "uuid")),
            uid: deep_str(&decrypted, "uid")
                .or_else(|| deep_str(&decrypted, "id"))
                .or_else(|| deep_str(scheme, "uid"))
                .or_else(|| deep_str(scheme, "id")),
        };
        if identity.uuid.is_none() && identity.uid.is_none() {
            return Err(StoreError::Parse(format!(
                "no identity in decrypted scheme: {decrypted}"
            )));
        }
        Ok(identity)
    }

    /// Accept the same required consents as the browser's terms modal.
    pub async fn accept_required_consents(&mut self, uuid: &str) -> StoreResult<()> {
        self.post(
            "/api/consents/set-consents-sixty60",
            contract::sixty60_terms_consent_body(uuid),
        )
        .await?;
        self.post(
            "/api/consents/set-consents-xtra-savings",
            contract::xtra_savings_rewards_consent_body(uuid),
        )
        .await?;
        Ok(())
    }

    /// Step 3: verify date of birth in the browser's `DD/MM/YYYY` format.
    pub async fn verify_date_of_birth(
        &mut self,
        date_of_birth: &str,
        scheme: &Value,
    ) -> StoreResult<()> {
        self.post(
            "/api/login/verify-date-of-birth",
            contract::verify_date_of_birth_body(date_of_birth, scheme),
        )
        .await?;
        Ok(())
    }

    pub async fn validate_id_or_passport_available(
        &mut self,
        id_or_passport: &str,
        uid: &str,
    ) -> StoreResult<bool> {
        let v = self
            .post(
                "/api/user/validate-if-id-or-passport-number-in-ciam",
                contract::validate_id_or_passport_body(id_or_passport, uid),
            )
            .await?;
        if str_at(&v, "/exception").as_deref() == Some("Customer not found") {
            return Ok(true);
        }
        if str_at(&v, "/response/uid")
            .filter(|uid| !uid.is_empty())
            .is_some()
        {
            return Ok(false);
        }
        Ok(true)
    }

    pub async fn update_customer_id_or_passport(&mut self, payload: Value) -> StoreResult<()> {
        self.post(
            "/api/user/update-customer-id-or-passport",
            contract::update_customer_id_or_passport_body(&payload),
        )
        .await?;
        Ok(())
    }

    /// Browser post-login bootstrap: hydrate the user/address/store context so
    /// later catalogue/cart calls have the same inputs as the web app.
    pub async fn bootstrap_login_context(&mut self) -> StoreResult<()> {
        let profile = self
            .post(
                "/api/user/get-user-profile",
                contract::get_user_profile_body(),
            )
            .await?;
        let user = profile
            .pointer("/response/user")
            .or_else(|| profile.get("user"))
            .ok_or_else(|| StoreError::Parse(format!("no user in profile response: {profile}")))?;
        let uid = str_at(user, "/uid")
            .or_else(|| str_at(user, "/id"))
            .ok_or_else(|| StoreError::Parse(format!("no uid in profile response: {profile}")))?;

        let on_demand = self
            .post(
                "/api/user/get-on-demand-profile",
                contract::get_on_demand_profile_body(&uid),
            )
            .await?;
        if bool_at(&on_demand, "/generateServiceTicket").unwrap_or(false) {
            return Err(StoreError::Parse(
                "login requires Checkers support profile linking".to_string(),
            ));
        }
        let sixty60_profile = on_demand
            .pointer("/userProfile")
            .or_else(|| on_demand.pointer("/response/userProfile"))
            .ok_or_else(|| {
                StoreError::Parse(format!("no on-demand profile in response: {on_demand}"))
            })?;

        let address_id = str_at(sixty60_profile, "/lastUsedAddress/identifier")
            .or_else(|| str_at(sixty60_profile, "/lastUsedAddress/id"))
            .unwrap_or_default();
        let addresses = self
            .post(
                "/api/address/get-user-addresses",
                contract::get_user_addresses_body(&address_id),
            )
            .await?;
        if bool_at(&addresses, "/isLimitedExperience").unwrap_or(false) {
            return Err(StoreError::Parse(
                "login account is in limited-experience address mode".to_string(),
            ));
        }
        let delivery_address = addresses
            .get("deliveryAddress")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| StoreError::Parse(format!("no delivery address: {addresses}")))?;

        let store = self
            .post(
                "/api/store/fetch-store-contexts?update=true",
                contract::fetch_store_contexts_body(&delivery_address, sixty60_profile),
            )
            .await?;
        if let Some(store_contexts) = store.get("storeContexts").filter(|v| !v.is_null()) {
            self.set_store_contexts(store_contexts);
        }
        Ok(())
    }

    // --- shopping -----------------------------------------------------------

    /// Search the catalogue; returns candidate products. Endpoint, body and
    /// response shape confirmed from the live authenticated app (cookie-auth;
    /// the BFF adds x-api-key/Cognito server-side).
    pub async fn search(&mut self, query: &str) -> StoreResult<Vec<Product>> {
        let store_contexts = self.store_contexts();
        let body = contract::catalogue_search_body(&store_contexts, query);
        let v = self
            .post("/api/catalogue/get-products-filter", body)
            .await?;
        Ok(parse_products(&v))
    }

    /// The user's store contexts, pulled from the `storeContexts` cookie
    /// (URL-encoded JSON) — required in the search/cart request bodies.
    fn store_contexts(&self) -> Value {
        for c in self.cookies.split("; ") {
            if let Some(val) = c.strip_prefix("storeContexts=") {
                if let Ok(v) = serde_json::from_str::<Value>(&percent_decode(val)) {
                    return v;
                }
            }
        }
        Value::Array(vec![])
    }

    fn set_store_contexts(&mut self, store_contexts: &Value) {
        let encoded = percent_encode(&store_contexts.to_string());
        self.merge_cookies(&format!("storeContexts={encoded}; Path=/"));
    }

    /// Add a product to the user's cart.
    pub async fn add_to_cart(
        &mut self,
        product_id: &str,
        quantity: u32,
        selected_weight_option_index: Option<usize>,
    ) -> StoreResult<()> {
        let store_contexts = self.store_contexts();
        if store_contexts.as_array().is_none_or(Vec::is_empty) {
            return Err(StoreError::Parse(
                "no store context available for cart update".to_string(),
            ));
        }

        let product = self.fetch_product(product_id, &store_contexts).await?;
        let service_option_id = str_at(&product, "/serviceOptionId")
            .or_else(|| {
                product
                    .pointer("/productServiceOptions/0/serviceOptionId")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| contract::SIXTY_MINUTE_DELIVERY.to_string());
        let carts = self.fetch_carts(&store_contexts).await?;

        let delivery_address_id = self.delivery_address_id().await.unwrap_or_default();
        let payload = contract::build_cart_update_payload(
            &carts,
            &product,
            &service_option_id,
            quantity,
            &delivery_address_id,
            &store_contexts,
            &line_item_id(product_id),
            selected_weight_option_index,
        )
        .map_err(StoreError::Parse)?;
        self.post(
            "/api/cart/update-cart",
            contract::update_cart_payload_body(&payload, false),
        )
        .await?;
        let refreshed_cart = self.fetch_cart().await?;
        if !contract::cart_response_contains_product(&refreshed_cart, product_id) {
            return Err(StoreError::Parse(format!(
                "cart update returned success, but product {product_id} was not present after fetch"
            )));
        }
        Ok(())
    }

    /// Fetch the current cart (raw JSON — shape confirmed on first live run).
    pub async fn fetch_cart(&mut self) -> StoreResult<Value> {
        let store_contexts = self.store_contexts();
        self.post(
            "/api/cart/fetch-cart",
            contract::fetch_cart_body(&store_contexts),
        )
        .await
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

    async fn fetch_product(
        &mut self,
        product_id: &str,
        store_contexts: &Value,
    ) -> StoreResult<Value> {
        let ids = [product_id];
        let v = self
            .post(
                "/api/catalogue/get-products-filter",
                contract::catalogue_product_ids_body(store_contexts, &ids),
            )
            .await?;
        find_products(&v)
            .and_then(|products| {
                products
                    .iter()
                    .find(|product| str_at(product, "/id").as_deref() == Some(product_id))
                    .cloned()
            })
            .ok_or_else(|| StoreError::Parse(format!("no product returned for {product_id}")))
    }

    async fn fetch_carts(&mut self, store_contexts: &Value) -> StoreResult<Vec<Value>> {
        let v = self
            .post(
                "/api/cart/fetch-cart",
                contract::fetch_cart_body(store_contexts),
            )
            .await?;
        Ok(contract::cart_items_from_fetch_response(&v))
    }

    async fn delivery_address_id(&mut self) -> StoreResult<String> {
        let profile = self
            .post(
                "/api/user/get-user-profile",
                contract::get_user_profile_body(),
            )
            .await?;
        let user = profile
            .pointer("/response/user")
            .or_else(|| profile.get("user"))
            .ok_or_else(|| StoreError::Parse(format!("no user in profile response: {profile}")))?;
        let uid = str_at(user, "/uid")
            .or_else(|| str_at(user, "/id"))
            .ok_or_else(|| StoreError::Parse(format!("no uid in profile response: {profile}")))?;
        let on_demand = self
            .post(
                "/api/user/get-on-demand-profile",
                contract::get_on_demand_profile_body(&uid),
            )
            .await?;
        let sixty60_profile = on_demand
            .pointer("/userProfile")
            .or_else(|| on_demand.pointer("/response/userProfile"))
            .ok_or_else(|| {
                StoreError::Parse(format!("no on-demand profile in response: {on_demand}"))
            })?;
        str_at(sixty60_profile, "/lastUsedAddress/identifier")
            .or_else(|| str_at(sixty60_profile, "/lastUsedAddress/id"))
            .ok_or_else(|| StoreError::Parse("no last-used delivery address".to_string()))
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

        let req =
            Request::new_with_init(&url, &init).map_err(|e| StoreError::Network(e.to_string()))?;
        let mut resp = Fetch::Request(req)
            .send()
            .await
            .map_err(|e| StoreError::Network(e.to_string()))?;

        let mut set_cookies = resp.headers().get_all("set-cookie").unwrap_or_default();
        if set_cookies.is_empty() {
            set_cookies = resp.headers().get_all("Set-Cookie").unwrap_or_default();
        }
        if set_cookies.is_empty() {
            if let Ok(Some(set_cookie)) = resp.headers().get("set-cookie") {
                set_cookies.push(set_cookie);
            }
        }
        for set_cookie in set_cookies {
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
            let Some(name) = pair.split('=').next() else {
                continue;
            };
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

/// Find the `products` array in the get-products-filter response (`{products:[…]}`,
/// possibly nested) and map each entry.
fn parse_products(v: &Value) -> Vec<Product> {
    find_products(v)
        .map(|arr| arr.iter().filter_map(product_from).collect())
        .unwrap_or_default()
}

fn find_products(v: &Value) -> Option<&Vec<Value>> {
    match v {
        Value::Object(m) => {
            if let Some(Value::Array(a)) = m.get("products") {
                return Some(a);
            }
            m.values().find_map(find_products)
        }
        Value::Array(a) => a.iter().find_map(find_products),
        _ => None,
    }
}

fn line_item_id(product_id: &str) -> String {
    let millis = js_sys::Date::now() as u64;
    let hash = product_id
        .bytes()
        .fold(millis ^ 0x9e37_79b9_7f4a_7c15, |acc, byte| {
            acc.rotate_left(5) ^ u64::from(byte)
        });
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (hash >> 32) as u32,
        (hash >> 16) as u16,
        hash & 0x0fff,
        (hash >> 12) & 0x0fff,
        hash & 0x0000_ffff_ffff_ffff
    )
}

/// Percent-decode a cookie value (the `storeContexts` cookie is URL-encoded JSON).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
    Some(Product {
        product_id,
        name,
        brand,
        price_cents,
        in_stock,
        variable_weight_options: v
            .get("variableWeightOptions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
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

fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn deep_str(v: &Value, key: &str) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(s) = map.get(key).and_then(Value::as_str) {
                return Some(s.to_string());
            }
            map.values().find_map(|v| deep_str(v, key))
        }
        Value::Array(values) => values.iter().find_map(|v| deep_str(v, key)),
        _ => None,
    }
}

fn bool_at(v: &Value, pointer: &str) -> Option<bool> {
    v.pointer(pointer).and_then(Value::as_bool)
}
