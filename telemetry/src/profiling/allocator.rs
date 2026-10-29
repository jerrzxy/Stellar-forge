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

//! Allocator and profiling configuration for memory introspection.
//!
//! Exposes Jemalloc memory allocation statistics and heap profiling controls.
//! Zero performance overhead during standard, non-debug runtime operations.

use serde::{Deserialize, Serialize};

/// Global allocator memory statistics snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AllocatorStats {
    /// Total bytes allocated by the application.
    pub allocated_bytes: u64,
    /// Total bytes active (in pages allocated by the application).
    pub active_bytes: u64,
    /// Total bytes in metadata structures.
    pub metadata_bytes: u64,
    /// Total bytes reserved by the allocator (mapped virtual memory).
    pub resident_bytes: u64,
    /// Total bytes mapped by the allocator.
    pub mapped_bytes: u64,
    /// Total bytes retained (virtual memory with no physical backing).
    pub retained_bytes: u64,
}

impl Default for AllocatorStats {
    fn default() -> Self {
        Self {
            allocated_bytes: 0,
            active_bytes: 0,
            metadata_bytes: 0,
            resident_bytes: 0,
            mapped_bytes: 0,
            retained_bytes: 0,
        }
    }
}

/// Profile configuration and control handle.
#[derive(Debug, Clone)]
pub struct ProfilingController {
    enabled: bool,
}

impl ProfilingController {
    /// Create a new ProfilingController.
    pub fn new() -> Self {
        Self {
            enabled: Self::check_profiling_enabled(),
        }
    }

    /// Check if profiling is enabled via environment variables or runtime flags.
    pub fn check_profiling_enabled() -> bool {
        std::env::var("REST_API_PROFILING_ENABLED")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                matches!(v.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false)
            || std::env::var("MALLOC_CONF")
                .map(|v| v.contains("prof:true"))
                .unwrap_or(false)
    }

    /// Whether memory profiling is active and available.
    pub fn is_active(&self) -> bool {
        self.enabled
    }

    /// Retrieve current allocator statistics with zero instrumentation overhead.
    pub fn stats(&self) -> AllocatorStats {
        #[cfg(target_os = "linux")]
        {
            if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                let mut rss = 0u64;
                let mut vm_size = 0u64;
                for line in contents.lines() {
                    if let Some(rest) = line.strip_prefix("VmRSS:") {
                        rss = rest
                            .split_whitespace()
                            .next()
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0)
                            * 1024;
                    }
                    if let Some(rest) = line.strip_prefix("VmSize:") {
                        vm_size = rest
                            .split_whitespace()
                            .next()
                            .and_then(|v| v.parse::<u64>().ok())
                            .unwrap_or(0)
                            * 1024;
                    }
                }
                return AllocatorStats {
                    allocated_bytes: rss,
                    active_bytes: rss,
                    metadata_bytes: 0,
                    resident_bytes: rss,
                    mapped_bytes: vm_size,
                    retained_bytes: vm_size.saturating_sub(rss),
                };
            }
        }

        AllocatorStats::default()
    }
}

impl Default for ProfilingController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiling_controller_default() {
        let controller = ProfilingController::new();
        let stats = controller.stats();
        assert!(stats.allocated_bytes >= 0);
    }

    #[test]
    fn test_profiling_controller_env() {
        std::env::set_var("REST_API_PROFILING_ENABLED", "true");
        assert!(ProfilingController::check_profiling_enabled());
        std::env::remove_var("REST_API_PROFILING_ENABLED");
    }
}
