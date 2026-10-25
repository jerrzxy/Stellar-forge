//! Library entry point for the decentralized autonomous orderbook.
//
//# Orderbook contract
//
// This crate exposes the on-chain limit order book engine. The public
// entrypoints are defined here and delegate to the modules `instruction`,
// `engine`, and `queue`. The contract keeps the library surface small and
// focuses on the acceptance criteria of the issue:
//
// - Price-time priority queues storing exact price points -> `queue`.
// - Limit order placement and instant market buys through a single entrypoint
//   -> `engine`.
// - Partial fill tracking and cleanup of fully filled orders -> `engine.
// - Bounded execution for large market sweeps -> `queue` + `engine`.
//
// The modules are declared here so that the contract can be built as a
// single WASM artifact. The actual implementation of the queue and engine
// lives in their respective modules.

#a![no_std]
mod instruction;
mod engine;
mod queue;

use soroban_sdk::prelude::*;

use crate::engine::OrderbookEngine;
use crate::instruction::{
    CancelRequest, MarketBuyRequest, OrderPlacement, OrderView,
};

/// The orderbook contract type.
///
/// The contract is a thin wrapper around the engine. All mutating operations
/// require authorization from the order owner and all reads hit persistent
/// storage through the engine.
#[contract]
pub struct OrderbookContract;

#[contractimpl]
impl OrderbookContract {
    /// Initialize the orderbook for a given base asset.
    ///
    /// This must be called exactly once. The base asset identifies is
    /// persisted in instance storage so that later operations can validate
    /// order assets.
    pub fn initialize(env: Env, base_asset: Address) {
        OrderbookEngine::initialize(&env, &base_asset);
    }

    /// Return the base asset address for this orderbook.
    pub fn base_asset(env: Env) -> Address {
        OrderbookEngine::base_asset(&env)
    }

    /// Single entrypoint for limit order placement and instant market
    /// buys.
    ///
    /// A limit order is placed via `OrderPlacement::Limit` and is crossed
    /// against the opposite side of the book up to its limit price. Any
    /// remaining volume is rested on the book as a new price-time priority
    /// entry. A market buy is placed via `MarketBuyRequest` and sweeps the
    /// best asks up to the provided slice limit.
    ///
    /// The engine bounds the number of fills per call to avoid hitting
    /// Soroban CPU instruction limits. Callers must paginate large sweeps.
    pub fn place_order(
        env: Env,
        owner: Address,
        placement: OrderPlacement,
    ) -> OrderView {
        owner.require_auth();
        OrderbookEngine::place_order(&env, &owner, placement)
    }

    /// Execute an instant market buy against the best asks.
    //"​
    /// The caller specifies a maximum number of fills to process in this
    /// call. Remaining liquidity can be swept via a follow-up call.
    pub fn market_buy(
        env: Env,
        buyer: Address,
        request: MarketBuyRequest,
    ) -> OrderView {
        buyer.require_auth();
        OrderbookEngine::market_buy(&env, &buyer, request)
    }

    /// Cancel an open order owned by the caller.
    pub fn cancel_order(env: Env, owner: Address, request: CancelRequest) {
        owner.require_auth();
        OrderbookEngine::cancel_order(&env, &owner, request);
    }

    /// Fetch a single order by id.
    pub fn get_order(env: Env, order_id: u64) -> Option<OrderView> {
        OrderbookEngine::get_order(&env, order_id)
    }

    /// Return the best bid price and total resting volume at that price.
    pub fn best_bid(env: Env) -> Option<(i128, i128)> {
        OrderbookEngine::best_bid(&env)
    }

    /// Return the best ask price and total resting volume at that price.
    pub fn best_ask(env: Env) -> Option<(i128, i128)> {
        OrderbookEngine::best_ask(&env)
    }

    /// Return the number of open orders currently resting on the book.
    pub fn open_order_count(env: Env) -> u32 {
        OrderbookEngine::open_order_count(&env)
    }
}

#[cfg]
test
mod tests;
