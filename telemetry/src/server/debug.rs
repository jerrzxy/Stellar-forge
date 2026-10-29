// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Internal debug routes including heap profiling and flamegraph dump endpoints.

use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use std::sync::Arc;

use crate::profiling::allocator::ProfilingController;

/// Error response for debug endpoints.
#[derive(Debug, Serialize)]
pub struct DebugErrorResponse {
    pub error: String,
    pub message: String,
}

/// Handler for `GET /debug/pprof/heap`.
///
/// Returns live heap profile / flamegraph compatible data.
/// Zero overhead when not actively requested.
pub async fn heap_profile_handler() -> Response {
    if !ProfilingController::check_profiling_enabled() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(CONTENT_TYPE, "application/json")],
            serde_json::to_string(&DebugErrorResponse {
                error: "profiling_disabled".to_string(),
                message: "Profiling is disabled. Set REST_API_PROFILING_ENABLED=true or MALLOC_CONF=prof:true to enable.".to_string(),
            })
            .unwrap_or_default(),
        )
            .into_response();
    }

    // In non-jemalloc or standard test builds, generate an introspection payload
    // indicating allocator footprint and allocation sites for flamegraph tooling.
    let controller = ProfilingController::new();
    let stats = controller.stats();

    // When requested via debug endpoint, format payload
    let payload = serde_json::json!({
        "version": "pprof/v1",
        "type": "heap",
        "unit": "bytes",
        "stats": stats,
        "captured_at": chrono::Utc::now().to_rfc3339(),
    });

    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/octet-stream"),
            (
                CONTENT_DISPOSITION,
                "attachment; filename=\"heap-profile.pb\"",
            ),
        ],
        payload.to_string(),
    )
        .into_response()
}

/// Attach debug routes to an existing Axum router.
pub fn debug_routes() -> Router {
    Router::new().route("/debug/pprof/heap", get(heap_profile_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    use axum::http::Request;

    #[tokio::test]
    async fn test_debug_heap_route_disabled() {
        std::env::remove_var("REST_API_PROFILING_ENABLED");
        std::env::remove_var("MALLOC_CONF");
        let app = debug_routes();
        let response = app
            .oneshot(Request::builder().uri("/debug/pprof/heap").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_debug_heap_route_enabled() {
        std::env::set_var("REST_API_PROFILING_ENABLED", "true");
        let app = debug_routes();
        let response = app
            .oneshot(Request::builder().uri("/debug/pprof/heap").body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        std::env::remove_var("REST_API_PROFILING_ENABLED");
    }
}
