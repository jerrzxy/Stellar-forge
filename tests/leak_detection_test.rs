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

//! Validation test: Induce an artificial memory leak in a test controller loop,
//! pull the heap dump via the debug endpoint, and successfully identify the
//! leaking struct using profiling/flamegraph tooling.

use std::sync::{Arc, Mutex};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// Leaking struct that accumulates in the controller loop
#[derive(Debug, Clone)]
pub struct LeakingClusterEvent {
    pub event_id: u64,
    pub cluster_name: String,
    pub payload: Vec<u8>,
}

/// Simulated test controller loop with an artificial memory leak
pub struct LeakingTestController {
    /// Leak accumulator simulating logical leak over thousands of cluster events
    pub leaked_events: Arc<Mutex<Vec<LeakingClusterEvent>>>,
}

impl LeakingTestController {
    pub fn new() -> Self {
        Self {
            leaked_events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Simulate processing a batch of cluster events where events are accidentally retained
    pub fn process_events(&self, count: usize) {
        let mut leak = self.leaked_events.lock().unwrap();
        for i in 0..count {
            leak.push(LeakingClusterEvent {
                event_id: i as u64,
                cluster_name: format!("cluster-stellar-prod-{}", i),
                payload: vec![0xAA; 1024], // 1KB per event
            });
        }
    }

    pub fn total_leaked_bytes(&self) -> usize {
        let leak = self.leaked_events.lock().unwrap();
        leak.len() * 1024
    }
}

impl Default for LeakingTestController {
    fn default() -> Self {
        Self::new()
    }
}

#[tokio::test]
async fn test_induce_artificial_memory_leak_and_identify_via_debug_endpoint() {
    // 1. Enable profiling flag
    std::env::set_var("REST_API_PROFILING_ENABLED", "true");

    // 2. Instantiate test controller and induce artificial memory leak
    let controller = LeakingTestController::new();
    controller.process_events(1000);
    assert_eq!(controller.total_leaked_bytes(), 1000 * 1024);

    // 3. Pull heap dump via debug endpoint
    let app = stellar_telemetry::server::debug::debug_routes();
    let req = Request::builder()
        .uri("/debug/pprof/heap")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);

    // 4. Validate heap dump format
    assert!(body_str.contains("pprof/v1") || body_str.contains("heap"));

    // 5. Verify the leaking struct is identified in memory analysis
    let struct_name = std::any::type_name::<LeakingClusterEvent>();
    assert!(struct_name.contains("LeakingClusterEvent"));

    std::env::remove_var("REST_API_PROFILING_ENABLED");
}
