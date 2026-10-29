//! Health probing for the Captive Core IPC endpoint.

use std::time::Duration;

use reqwest::Client;

/// Probe a Captive Core/RPC health endpoint with a bounded timeout.
pub async fn probe_http(client: &Client, endpoint: &str, timeout: Duration) -> Result<(), String> {
    client
        .get(endpoint)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| error.to_string())?
        .error_for_status()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Alias for [`probe_http`] using IPC terminology.
pub async fn probe_ipc(client: &Client, endpoint: &str, timeout: Duration) -> Result<(), String> {
    probe_http(client, endpoint, timeout).await
}

/// Convenience probe that owns its HTTP client.
pub async fn probe_endpoint(endpoint: &str, timeout: Duration) -> Result<(), String> {
    let client = Client::new();
    probe_http(&client, endpoint, timeout).await
}

/// Backwards-friendly name used by some callers.
pub async fn check_ipc(client: &Client, endpoint: &str, timeout: Duration) -> Result<(), String> {
    probe_http(client, endpoint, timeout).await
}
