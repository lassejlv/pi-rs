//! Regression for the wasm32 timer seams: a sleep dropped before it fires must
//! clear its `setTimeout` (an armed timer kept Durable Objects from being
//! evicted), and a sleep that fired must not clear anything. The page wraps
//! `clearTimeout` to count calls before invoking this.
use std::time::Duration;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub async fn sleep_drop_probe() {
    // Two pending sleeps per seam, dropped unfired: four clearTimeout calls.
    drop(pi_chord::rt::sleep(Duration::from_secs(600)));
    drop(pi_chord::rt::sleep_until(pi_chord::rt::Instant::now() + Duration::from_secs(600)));
    drop(pi_ai::utils::wasm::sleep(Duration::from_secs(600)));
    drop(pi_ai::utils::wasm::sleep_until(pi_ai::utils::wasm::Instant::now() + Duration::from_secs(600)));
    // A timeout whose future wins drops its timer too: one more call each.
    let _ = pi_chord::rt::timeout(Duration::from_secs(600), async {}).await;
    let _ = pi_ai::utils::wasm::timeout(Duration::from_secs(600), async {}).await;
    // Sleeps that fire release nothing.
    pi_chord::rt::sleep(Duration::from_millis(1)).await;
    pi_ai::utils::wasm::sleep(Duration::from_millis(1)).await;
}
