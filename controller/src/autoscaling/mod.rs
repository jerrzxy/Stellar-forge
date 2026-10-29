// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Latency-driven autoscaling for Soroban RPC replicas.
//!
//! - [`wasm_metrics`] — histogram tracking WASM host-function execution time,
//!   exposing p50/p95/p99 as the `soroban_wasm_latency_ms` metric.
//! - [`hpa_adapter`] — custom-metrics adapter that serves the metric to the
//!   Kubernetes HPA, plus the sustained-breach / cool-down scaling policy.

pub mod hpa_adapter;
pub mod wasm_metrics;
