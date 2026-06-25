//! Checkers Sixty60 store adapter — STUB.
//!
//! Intentionally unimplemented. The whole approach is gated on the spike
//! (see the Inkdrop spec §7): can we capture a Sixty60 session and add an item
//! to a cart *from Cloudflare's egress, without being flagged*? Until a real
//! traffic capture (HAR / mitmproxy of the Sixty60 app) tells us the auth + cart
//! API shape, there is nothing concrete to implement here.
//!
//! This module exists to pin the trait the engine will depend on, so the
//! engine can be built against it before the integration is proven.

use crate::protocol::CartItem;

#[derive(Debug)]
pub enum StoreError {
    /// The captured session is gone/expired — the user must re-auth.
    SessionExpired,
    /// We were blocked (bot detection / rate limit). The egress risk made real.
    Blocked,
    /// The integration isn't built yet.
    NotImplemented,
    Other(String),
}

/// A product the store can actually sell, resolved from a fuzzy item name.
#[derive(Debug, Clone)]
pub struct Sku {
    pub sku_id: String,
    pub name: String,
    pub price_cents: u64,
    pub in_stock: bool,
}

/// What the engine needs from any store. Sixty60 is the first implementor;
/// keeping it a trait means PnP asap! / Woolies Dash slot in later unchanged.
pub trait StoreAdapter {
    /// Resolve a free-text item ("2L full cream milk") to candidate SKUs.
    fn search(&self, query: &str) -> Result<Vec<Sku>, StoreError>;
    /// Add a resolved SKU to the user's cart, acting as the user.
    fn add_to_cart(&self, sku_id: &str, qty: u32) -> Result<(), StoreError>;
    /// Current cart contents.
    fn cart(&self) -> Result<Vec<CartItem>, StoreError>;
    /// Deep link into the native app for the user to confirm + pay (hand-off).
    fn checkout_url(&self) -> Result<String, StoreError>;
}

/// Placeholder Sixty60 client. Holds the (decrypted, in-memory) session token.
pub struct Sixty60 {
    #[allow(dead_code)]
    session_token: String,
}

impl Sixty60 {
    pub fn new(session_token: impl Into<String>) -> Self {
        Self { session_token: session_token.into() }
    }
}

impl StoreAdapter for Sixty60 {
    fn search(&self, _query: &str) -> Result<Vec<Sku>, StoreError> {
        Err(StoreError::NotImplemented)
    }
    fn add_to_cart(&self, _sku_id: &str, _qty: u32) -> Result<(), StoreError> {
        Err(StoreError::NotImplemented)
    }
    fn cart(&self) -> Result<Vec<CartItem>, StoreError> {
        Err(StoreError::NotImplemented)
    }
    fn checkout_url(&self) -> Result<String, StoreError> {
        Err(StoreError::NotImplemented)
    }
}
