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
//! Load test required by the issue #292 review process: the bridge must
//! sustain 5,000 events per second without building memory backpressure.
//!
//! Run the full review-scale version (1M events) explicitly:
//!
//! ```text
//! cargo test -p stellar-event-bridge --test load_test -- --ignored --nocapture
//! ```
//!
//! or via the worker binary:
//!
//! ```text
//! cargo run --release -p stellar-event-bridge --bin stellar-event-bridge -- \
//!     load-test --events 1000000 --target-eps 5000
//! ```
//!
//! A CI-sized subset runs on every `cargo test` in `src/bridge.rs`.

use stellar_event_bridge::bridge::run_load_test;

/// Review-scale run: 1,000,000 events sustained at >= 5,000 events/sec.
#[tokio::test]
#[ignore = "review-scale load test; run with --ignored --nocapture"]
async fn load_test_1m_events_at_5k_eps() {
    let report = run_load_test(1_000_000, 5_000.0, 4, 1).await;

    println!(
        "produced {} events across {} ledgers in {:.2}s = {:.0} events/sec (target {:.0})",
        report.events, report.ledgers, report.elapsed_secs, report.events_per_sec, report.target_eps
    );

    assert!(report.events >= 1_000_000, "must produce at least 1M events");
    assert!(
        report.passed,
        "bridge sustained only {:.0} events/sec; required {}",
        report.events_per_sec, report.target_eps
    );
}

/// Higher-cardinality partition-key mix (100 assets / 100 contracts), still
/// review scale.
#[tokio::test]
#[ignore = "review-scale load test; run with --ignored --nocapture"]
async fn load_test_high_cardinality_keys() {
    let report = run_load_test(1_000_000, 5_000.0, 32, 8).await;

    println!(
        "produced {} events across {} ledgers in {:.2}s = {:.0} events/sec",
        report.events, report.ledgers, report.elapsed_secs, report.events_per_sec
    );

    assert!(
        report.passed,
        "bridge sustained only {:.0} events/sec; required {}",
        report.events_per_sec, report.target_eps
    );
}
