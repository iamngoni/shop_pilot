//! `UserSession` Durable Object — one instance per (channel, user_id),
//! addressed by `idFromName(user_key)`. Owns the user's conversation state and
//! in-flight cart draft so the engine has somewhere durable to keep context
//! between turns.
//!
//! Minimal for now: a single opaque JSON blob under the key "state". The engine
//! will grow this into typed conversation state + pending-choice tracking.

use worker::*;

#[durable_object]
pub struct UserSession {
    state: State,
    #[allow(dead_code)]
    env: Env,
}

impl DurableObject for UserSession {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let url = req.url()?;
        match (req.method(), url.path()) {
            (Method::Get, "/state") => {
                let stored = self
                    .state
                    .storage()
                    .get::<String>("state")
                    .await
                    .unwrap_or_else(|_| "{}".to_string());
                Response::ok(stored)
            }
            (Method::Post, "/state") => {
                let body = req.text().await.unwrap_or_default();
                self.state.storage().put("state", body).await?;
                Response::ok("ok")
            }
            _ => Response::error("not found", 404),
        }
    }
}
