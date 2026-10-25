use soroban_sdk::contracterror;

/// Errors returned by the decentralized orderbook engine.
///
/// Error codes are stable and start at 1 to avoid conflicting with the
/// implicit success value (0) of Soroban contract results.
#[contracterror]
pub enum OrderbookError {
    /// The orderbook has not been initialized yet.
    NotInitialized = 1,

    /// The orderbook has already been initialized.
    AlreadyInitialized = 2,

    /// An order was submitted with a non-positive amount.
    InvalidAmount = 3,

    /// An order was submitted with a non-positive price.
    InvalidPrice = 4,

    /// The caller is not authorized to perform the operation.
    Unauthorized = 5,

    /// The referenced order does not exist.
    OrderNotFound = 6,

    /// The order has already been fully filled or cancelled.
    OrderNotOpen = 7,

    /// The caller is not the owner of the order being modified.
    NotOrderOwner = 8,

    /// The market buy could not be fully filled within the bounded execution
    /// budget; the remainder is reported to the caller.
    ExecutionBudgetExceeded = 9,

    /// The number of matches per call exceeds the configured maximum.
    MaxMatchesExceeded = 10,

    /// The price level index is out of bounds.
    PriceLevelNotFound = 11,

    /// The queue operation would exceed the allowed storage budget.
    QueueCapacityExceeded = 12,

    /// A pagination cursor was provided that does not match the current
    /// orderbook state.
    InvalidCursor = 13,

    /// The orderbook is closed and rejecting new orders.
    OrderbookClosed = 14,

    /// An arithmetic operation overflowed or underflowed.
    ArithmeticOverflow = 15,

    /// The submitted order would cross itself or violate self-trade protection.
    SelfTradeForbidden = 16,
}
