// Placeholder bin so `cargo build` has a target for the rlib half of the crate.
// The real entrypoint is the wasm `#[event(fetch)]` handler in worker_app.rs.
fn main() {
    println!("shop_pilot worker — deploy with `worker-build`");
}
