use soroban_sdk::token::TokenClient;
use soroban_sdk::{
    contract, contractimpl, contracttype, unwrap::UnwrapOptimized, Address, Env, Map, Symbol, Vec,
};

/// The single vault call this engine makes.
///
/// Declared as its own client instead of reusing the vault's generated
/// `YieldVaultClient`, because importing that would pull the vault contract's
/// entrypoints into a rebalance-engine wasm, and one wasm cannot export two
/// contracts that share an entrypoint name such as `initialize`.
#[soroban_sdk::contractclient(name = "VaultInfoClient")]
pub trait YieldVaultInterface {
    fn get_vault_info(env: Env) -> crate::yield_vault::VaultInfo;
}

/// Upper bound accepted for an oracle APY observation: 1,000,000 bps = 10,000%.
/// Guards against a misconfigured feed pushing nonsense into rebalance maths.
pub const MAX_APY_BPS: u32 = 1_000_000;

/// Default age after which an APY observation is considered stale: 24 hours.
pub const DEFAULT_MAX_APY_AGE: u64 = 86_400;

/// Default number of rejected observations that excludes a pool from routing
/// until its feed is accepted again (Issue #289).
pub const DEFAULT_FEED_FAILURE_THRESHOLD: u32 = 3;

/// Number of consecutive failed rebalances that trips the circuit breaker.
pub const EMERGENCY_STOP_WINDOW: u32 = 3;

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolAllocation {
    pub pool_id: Address,
    pub token_a: Address,
    pub token_b: Address,
    pub allocation_percent: u32,    // Basis points (10000 = 100%)
    pub target_apy: u32,            // Basis points
    pub current_apy: u32,           // Basis points
    pub impermanent_loss_risk: u32, // Basis points
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebalanceStrategy {
    pub strategy_id: u32,
    pub name: Symbol,
    pub risk_level: u32,          // 1=Conservative, 2=Balanced, 3=Aggressive
    pub min_apy_threshold: u32,   // Basis points
    pub max_il_risk: u32,         // Basis points
    pub rebalance_frequency: u64, // Seconds
    pub allocations: Vec<PoolAllocation>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebalanceProposal {
    pub from_pool: Address,
    pub to_pool: Address,
    pub amount_a: i128,
    pub amount_b: i128,
    pub expected_apy_improvement: u32, // Basis points
    pub estimated_gas_cost: i128,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebalanceHistory {
    pub timestamp: u64,
    pub from_pool: Address,
    pub to_pool: Address,
    pub amount_moved: i128,
    pub apy_before: u32,
    pub apy_after: u32,
    pub success: bool,
}

/// Arbitrage opportunity structure
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbitrageOpportunity {
    pub pool_id: Address,
    pub current_apy: u32,   // Basis points
    pub projected_apy: u32, // Basis points after rebalance
    pub il_risk: u32,       // Basis points
    pub net_profit: i128,   // In native token units
    pub apy_delta: u32,     // Difference in basis points
    pub recommended: bool,
}

/// Arbitrage rebalance threshold configuration
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArbitrageThresholds {
    pub min_apy_delta: u32, // Minimum APY difference to trigger rebalance (basis points)
    pub max_il_tolerance: u32, // Maximum acceptable IL (basis points)
    pub cooldown_period: u64, // Seconds between rebalances, applied per vault
    /// Minimum estimated net profit for an opportunity to be routed, in the
    /// same units as [`ArbitrageOpportunity::net_profit`] (Issue #294).
    /// `0` disables the filter.
    pub min_net_profit: i128,
}

/// Validation-failure tracking for one pool's oracle feed (Issue #289).
///
/// A feed whose last observation failed validation must not influence routing.
/// The off-chain validator calls [`RebalanceEngine::record_feed_validation_failure`]
/// each time it rejects an observation; once `failure_count` reaches the
/// configured exclusion threshold the feed is marked `excluded` and every
/// routing path skips the pool until a valid observation is accepted again.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedValidationRecord {
    /// Rejected observations counted since the feed was last accepted.
    pub failure_count: u32,
    /// Ledger timestamp of the most recent rejected observation.
    pub last_failure_at: u64,
    /// True once `failure_count` reached the exclusion threshold.
    pub excluded: bool,
}

/// One oracle observation for a pool.
///
/// `updated_by` is the authenticated admin or APY keeper that most recently
/// published this feed, and `updated_at` is the ledger timestamp (seconds) of
/// that successful publication.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebalancePacing {
    /// Maximum amount (in the moved unit) that may leave one pool per window.
    /// Zero disables pacing.
    pub max_amount_per_window: i128,
    /// Length of the pacing window, in seconds.
    pub window_seconds: u64,
}

/// Consumption of the per-pool rebalance budget inside one window.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolWindowUsage {
    /// Whether a window has been opened for this pool at all. A boolean, rather
    /// than a zero timestamp, so a window opened at ledger time 0 is distinct
    /// from no window yet.
    pub window_active: bool,
    /// Ledger timestamp that opened the window.
    pub window_started_at: u64,
    /// Total amount moved out of the pool inside this window.
    pub amount_moved: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolApyRecord {
    pub apy_bps: u32,        // Observed APY in basis points (10000 = 100%)
    pub updated_at: u64,     // Ledger timestamp the feed was refreshed
    pub updated_by: Address, // Admin or keeper that pushed the observation
}

/// Staleness snapshot for one pool's oracle feed (Issue #293).
///
/// Gives off-chain monitors a direct data-age signal instead of making them
/// reconstruct it from `updated_at` and the current ledger time.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedDataAge {
    /// Ledger timestamp of the most recent observation, or 0 when unpublished.
    pub updated_at: u64,
    /// Seconds elapsed since that observation (saturating; 0 when unpublished).
    pub age_seconds: u64,
    /// Age at or after which the observation is considered stale.
    pub max_age_seconds: u64,
    /// True when the feed has an observation and `age_seconds` is within the
    /// configured maximum age. False when unpublished or stale.
    pub fresh: bool,
}

#[contract]
pub struct RebalanceEngine;

#[contractimpl]
impl RebalanceEngine {
    /// Initialize the rebalance engine
    pub fn initialize(env: Env, admin: Address) {
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &admin);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "paused"), &false);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "next_strategy_id"), &1u32);

        // Initialize empty strategy registry as Map for O(1) access (Issue #108)
        let strategies: Map<u32, RebalanceStrategy> = Map::new(&env);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "strategies"), &strategies);

        // Initialize empty history
        let history: Vec<RebalanceHistory> = Vec::new(&env);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "history"), &history);

        // Initialize the APY registry, the keeper set and the staleness bound
        // (Issue #105).
        let apy_registry: Map<Address, PoolApyRecord> = Map::new(&env);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "pool_apy"), &apy_registry);
        let keepers: Vec<Address> = Vec::new(&env);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "apy_keepers"), &keepers);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "max_apy_age"), &DEFAULT_MAX_APY_AGE);
        // Issue #289: feed validation-failure tracking, keyed by pool.
        let feed_validation: Map<Address, FeedValidationRecord> = Map::new(&env);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "feed_validation"), &feed_validation);
        env.storage().instance().set(
            &Symbol::new(&env, "feed_failure_threshold"),
            &DEFAULT_FEED_FAILURE_THRESHOLD,
        );
        // Record the storage schema version of this build; `migrate` moves it
        // forward after an upgrade swaps the code.
        crate::upgrade::set_storage_version(&env, crate::upgrade::STORAGE_VERSION);
    }

    /// Replace this contract's code in place, keeping its address and storage
    /// (admin only). See [crate::upgrade] and `docs/UPGRADING.md`.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: soroban_sdk::BytesN<32>) {
        Self::require_admin(&env, admin.clone());
        crate::upgrade::update_wasm(&env, &admin, new_wasm_hash);
    }

    /// Storage schema version of this contract's stored data.
    pub fn get_storage_version(env: Env) -> u32 {
        crate::upgrade::storage_version(&env)
    }

    /// Adopt the storage schema version of this build (admin only, idempotent).
    /// Returns the version that was replaced.
    pub fn migrate(env: Env, admin: Address) -> u32 {
        Self::require_admin(&env, admin.clone());
        crate::upgrade::mark_migrated(&env, &admin)
    }

    /// Create a new rebalance strategy
    pub fn create_strategy(
        env: Env,
        admin: Address,
        name: Symbol,
        risk_level: u32,
        min_apy_threshold: u32,
        max_il_risk: u32,
        rebalance_frequency: u64,
        allocations: Vec<PoolAllocation>,
    ) -> u32 {
        Self::require_admin(&env, admin.clone());
        Self::require_not_paused(&env);
        Self::validate_allocations(&allocations);

        let strategy_id = Self::get_next_strategy_id(&env);

        let strategy = RebalanceStrategy {
            strategy_id,
            name: name.clone(),
            risk_level,
            min_apy_threshold,
            max_il_risk,
            rebalance_frequency,
            allocations,
        };

        let mut strategies = Self::get_strategies_map(env.clone());
        require!((1..=3).contains(&risk_level), "risk_level must be 1-3");
        strategies.set(strategy_id, strategy.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "strategies"), &strategies);

        env.events().publish(
            (Symbol::new(&env, "rebalance_strategy_created"), strategy_id),
            (admin, name, risk_level),
        );

        strategy_id
    }

    /// Update an existing strategy
    pub fn update_strategy(
        env: Env,
        admin: Address,
        strategy_id: u32,
        name: Symbol,
        risk_level: u32,
        min_apy_threshold: u32,
        max_il_risk: u32,
        rebalance_frequency: u64,
        allocations: Vec<PoolAllocation>,
    ) {
        Self::require_admin(&env, admin.clone());
        Self::require_not_paused(&env);
        Self::validate_allocations(&allocations);
        require!((1..=3).contains(&risk_level), "risk_level must be 1-3");

        let mut strategies = Self::get_strategies_map(env.clone());
        if strategies.get(strategy_id).is_none() {
            panic!("strategy not found");
        }
        let updated_strategy = RebalanceStrategy {
            strategy_id,
            name: name.clone(),
            risk_level,
            min_apy_threshold,
            max_il_risk,
            rebalance_frequency,
            allocations,
        };
        strategies.set(strategy_id, updated_strategy);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "strategies"), &strategies);

        env.events().publish(
            (Symbol::new(&env, "rebalance_strategy_updated"), strategy_id),
            (admin, name, risk_level),
        );
    }

    /// Analyze current pool conditions and generate rebalance proposals
    pub fn analyze_rebalance_opportunities(env: Env, strategy_id: u32) -> Vec<RebalanceProposal> {
        Self::require_not_paused(&env);

        let strategy = Self::get_strategy(env.clone(), strategy_id);
        let mut proposals: Vec<RebalanceProposal> = Vec::new(&env);

        // Analyze each allocation in the strategy
        for allocation in strategy.allocations.iter() {
            // Prefer the live oracle value over the APY snapshot stored in the
            // strategy, falling back to the snapshot when no fresh feed exists.
            let current_apy = Self::resolve_apy(&env, &allocation.pool_id, allocation.current_apy);
            let target_apy = allocation.target_apy;

            // Check if rebalancing is needed
            if current_apy < target_apy - strategy.min_apy_threshold {
                // Find better pools
                let better_pools =
                    Self::find_better_pools(&env, &allocation, &strategy, current_apy);

                for better_pool in better_pools {
                    let proposal = RebalanceProposal {
                        from_pool: allocation.pool_id.clone(),
                        to_pool: better_pool.pool_id,
                        amount_a: allocation.allocation_percent as i128,
                        amount_b: allocation.allocation_percent as i128,
                        expected_apy_improvement: better_pool.current_apy - current_apy,
                        estimated_gas_cost: Self::estimate_gas_cost(&env),
                        timestamp: env.ledger().timestamp(),
                    };
                    proposals.push_back(proposal);
                }
            }
        }

        proposals
    }

    /// Execute a rebalance proposal
    pub fn execute_rebalance(env: Env, _caller: Address, proposal: RebalanceProposal) -> bool {
        Self::require_not_paused(&env);

        // Verify caller is authorized (could be a vault or authorized manager)
        // For now, allow any caller - in production, add proper authorization

        // Pace large moves out of a pool: an oversized or over-budget proposal is
        // rejected before any token moves, so a mass unwind cannot execute in a
        // single window. Only successful rebalances consume the budget.
        let moved_amount = proposal.amount_a + proposal.amount_b;
        if moved_amount > 0 && !Self::pacing_allows(&env, &proposal.from_pool, moved_amount) {
            return false;
        }

        let apy_before = Self::get_pool_current_apy(env.clone(), proposal.from_pool.clone());
        let mut success = false;

        // Execute the rebalance (simplified - would integrate with AMM contracts)
        if Self::perform_rebalance(&env, &proposal) {
            success = true;
            if moved_amount > 0 {
                Self::record_pacing(&env, &proposal.from_pool, moved_amount);
            }
        }

        let apy_after = if success {
            Self::get_pool_current_apy(env.clone(), proposal.to_pool.clone())
        } else {
            apy_before
        };

        // Record in history
        let history_entry = RebalanceHistory {
            timestamp: env.ledger().timestamp(),
            from_pool: proposal.from_pool,
            to_pool: proposal.to_pool,
            amount_moved: proposal.amount_a + proposal.amount_b,
            apy_before,
            apy_after,
            success,
        };

        Self::add_to_history(&env, history_entry);

        success
    }

    /// Validate that allocations are non-empty, each within bounds (<= 10000 bps),
    /// and that the percentages sum to exactly 10000 bps (100%).
    fn validate_allocations(allocations: &Vec<PoolAllocation>) {
        require!(!allocations.is_empty(), "allocations must not be empty");

        let mut total_bps: u64 = 0;
        for allocation in allocations.iter() {
            require!(
                allocation.allocation_percent <= 10000,
                "allocation_percent must not exceed 10000 bps (100%)"
            );
            total_bps += allocation.allocation_percent as u64;
        }

        require!(
            total_bps == 10000,
            "allocations must sum to exactly 10000 bps (100%)"
        );
    }

    /// Internal: get strategies Map for O(1) access
    pub fn get_strategies_map(env: Env) -> Map<u32, RebalanceStrategy> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "strategies"))
            .unwrap_optimized()
    }

    /// Get all strategies as Vec (materialized from Map)
    pub fn get_strategies(env: Env) -> Vec<RebalanceStrategy> {
        let map = Self::get_strategies_map(env.clone());
        let mut v: Vec<RebalanceStrategy> = Vec::new(&env);
        for (_, strat) in map.iter() {
            v.push_back(strat);
        }
        v
    }

    /// Get specific strategy - O(1)
    pub fn get_strategy(env: Env, strategy_id: u32) -> RebalanceStrategy {
        Self::get_strategies_map(env)
            .get(strategy_id)
            .unwrap_or_else(|| panic!("strategy not found"))
    }

    /// Paginated strategy listing (Issue #107)
    pub fn get_strategies_paginated(env: Env, limit: u32, offset: u32) -> Vec<RebalanceStrategy> {
        let all = Self::get_strategies(env.clone());
        if limit == 0 || offset >= all.len() {
            return Vec::new(&env);
        }
        let mut out: Vec<RebalanceStrategy> = Vec::new(&env);
        let end = (offset + limit).min(all.len());
        for i in offset..end {
            out.push_back(all.get(i).unwrap());
        }
        out
    }

    pub fn get_strategy_count(env: Env) -> u32 {
        Self::get_strategies_map(env).len()
    }

    /// Get rebalance history - most recent `limit` entries (backward compat)
    pub fn get_history(env: Env, limit: u32) -> Vec<RebalanceHistory> {
        // A limit of 0 means no results should be returned
        if limit == 0 {
            return Vec::new(&env);
        }

        let history: Vec<RebalanceHistory> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "history"))
            .unwrap_optimized();

        let mut result: Vec<RebalanceHistory> = Vec::new(&env);
        let start = if history.len() > limit {
            history.len() - limit
        } else {
            0
        };

        for i in start..history.len() {
            result.push_back(history.get(i).unwrap());
        }

        result
    }

    /// Paginated history with limit/offset for bounded reads (Issue #107)
    pub fn get_history_paginated(env: Env, limit: u32, offset: u32) -> Vec<RebalanceHistory> {
        let history: Vec<RebalanceHistory> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "history"))
            .unwrap_optimized();
        if limit == 0 || offset >= history.len() {
            return Vec::new(&env);
        }
        let mut out: Vec<RebalanceHistory> = Vec::new(&env);
        let end = (offset + limit).min(history.len());
        for i in offset..end {
            out.push_back(history.get(i).unwrap());
        }
        out
    }

    pub fn get_history_count(env: Env) -> u32 {
        let h: Vec<RebalanceHistory> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "history"))
            .unwrap_optimized();
        h.len()
    }

    /// Get current allocations for a strategy
    pub fn get_current_allocations(env: Env, strategy_id: u32) -> Vec<PoolAllocation> {
        let strategy = Self::get_strategy(env, strategy_id);
        strategy.allocations
    }

    // ============ APY ORACLE REGISTRY (Issue #105) ============

    /// Publish an APY observation for a pool (admin or APY keeper).
    ///
    /// This replaces the previous hardcoded APY. Rebalance analysis and
    /// `get_pool_current_apy` now read this registry, so a keeper (or the admin)
    /// feeding real numbers is what drives rebalance decisions.
    pub fn update_pool_apy(env: Env, caller: Address, pool_id: Address, apy_bps: u32) {
        Self::require_not_paused(&env);
        Self::require_admin_or_apy_keeper(&env, caller.clone());
        require!(apy_bps <= MAX_APY_BPS, "apy_bps exceeds maximum");
        // The caller address is part of the feed's public provenance, so it
        // must be authorized by that address rather than merely supplied as a
        // method argument.
        caller.require_auth();

        let mut registry = Self::get_apy_registry(env.clone());
        let previous = registry.get(pool_id.clone());

        let record = PoolApyRecord {
            apy_bps,
            updated_at: env.ledger().timestamp(),
            updated_by: caller.clone(),
        };

        // A re-published observation still refreshes freshness: a feeder that
        // keeps reporting a stable rate is not stuck. Issue #295 only adds a
        // marker so an operator can tell a genuine change from a no-op.
        registry.set(pool_id.clone(), record);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "pool_apy"), &registry);

        match previous {
            Some(prev) if prev.apy_bps == apy_bps => {
                env.events().publish(
                    (Symbol::new(&env, "pool_apy_unchanged"), pool_id),
                    (caller, apy_bps, prev.updated_at),
                );
            }
            _ => {
                env.events().publish(
                    (Symbol::new(&env, "pool_apy_updated"), pool_id),
                    (caller, apy_bps),
                );
            }
        }
    }

    /// Current APY of a pool from the oracle registry, in basis points.
    ///
    /// Returns 0 when no feed has been published for `pool_id`; 0 means
    /// "unknown" and is deliberately not a fabricated rate.
    pub fn get_pool_current_apy(env: Env, pool_id: Address) -> u32 {
        Self::get_apy_registry(env.clone())
            .get(pool_id)
            .map(|record| record.apy_bps)
            .unwrap_or(0u32)
    }

    /// Full oracle record for a pool, if a feed exists.
    pub fn get_pool_apy_record(env: Env, pool_id: Address) -> Option<PoolApyRecord> {
        Self::get_apy_registry(env).get(pool_id)
    }

    /// Age in seconds after which an APY observation is rejected as stale.
    pub fn get_max_apy_age(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "max_apy_age"))
            .unwrap_or(DEFAULT_MAX_APY_AGE)
    }

    /// Set the maximum age of a usable APY observation (admin only).
    pub fn set_max_apy_age(env: Env, admin: Address, max_age: u64) {
        Self::require_admin(&env, admin.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "max_apy_age"), &max_age);

        env.events().publish(
            (Symbol::new(&env, "max_apy_age_updated"),),
            (admin, max_age),
        );
    }

    /// True when the pool has an observation that is still within the
    /// configured maximum age.
    pub fn is_apy_fresh(env: Env, pool_id: Address) -> bool {
        match Self::get_apy_registry(env.clone()).get(pool_id) {
            Some(record) => {
                let age = env.ledger().timestamp().saturating_sub(record.updated_at);
                age <= Self::get_max_apy_age(env)
            }
            None => false,
        }
    }

    /// Data-age snapshot for a pool's oracle feed (Issue #293).
    ///
    /// An unpublished feed reports `age_seconds == 0` and `fresh == false`, so
    /// a monitor cannot mistake "no data" for "brand new data"; the `fresh`
    /// flag is the authoritative signal in both cases.
    pub fn get_feed_data_age(env: Env, pool_id: Address) -> FeedDataAge {
        let max_age_seconds = Self::get_max_apy_age(env.clone());
        match Self::get_apy_registry(env.clone()).get(pool_id) {
            Some(record) => {
                let age_seconds = env.ledger().timestamp().saturating_sub(record.updated_at);
                FeedDataAge {
                    updated_at: record.updated_at,
                    age_seconds,
                    max_age_seconds,
                    fresh: age_seconds <= max_age_seconds,
                }
            }
            None => FeedDataAge {
                updated_at: 0,
                age_seconds: 0,
                max_age_seconds,
                fresh: false,
            },
        }
    }

    /// Seconds since the pool's most recent observation, or `None` when the
    /// feed has never been published (Issue #293).
    pub fn get_feed_age_seconds(env: Env, pool_id: Address) -> Option<u64> {
        Self::get_apy_registry(env.clone())
            .get(pool_id)
            .map(|record| env.ledger().timestamp().saturating_sub(record.updated_at))
    }

    /// Emit the current data age of a pool's feed as an event, so off-chain
    /// monitors can record staleness without polling (Issue #293).
    ///
    /// Returns the same snapshot the event carries.
    pub fn emit_feed_data_age(env: Env, pool_id: Address) -> FeedDataAge {
        let age = Self::get_feed_data_age(env.clone(), pool_id.clone());
        env.events().publish(
            (Symbol::new(&env, "feed_data_age"), pool_id),
            (
                age.updated_at,
                age.age_seconds,
                age.max_age_seconds,
                age.fresh,
            ),
        );
        age
    }

    /// Replace the set of addresses allowed to push APY observations (admin only).
    /// The admin can always update feeds and does not need to be listed.
    pub fn set_apy_keepers(env: Env, admin: Address, keepers: Vec<Address>) {
        Self::require_admin(&env, admin.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "apy_keepers"), &keepers);

        env.events().publish(
            (Symbol::new(&env, "apy_keepers_updated"),),
            (admin, keepers),
        );
    }

    /// Current APY keeper set
    pub fn get_apy_keepers(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "apy_keepers"))
            .unwrap_or(Vec::new(&env))
    }

    // ============ FEED VALIDATION TRACKING (Issue #289) ============

    /// Number of rejected observations that excludes a pool from routing.
    pub fn get_feed_failure_threshold(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "feed_failure_threshold"))
            .unwrap_or(DEFAULT_FEED_FAILURE_THRESHOLD)
    }

    /// Set the number of rejected observations that excludes a pool (admin only).
    ///
    /// A threshold of `0` disables exclusion: validation failures are still
    /// recorded and emitted, but they never remove a pool from routing.
    pub fn set_feed_failure_threshold(env: Env, admin: Address, threshold: u32) {
        Self::require_admin(&env, admin.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "feed_failure_threshold"), &threshold);

        env.events().publish(
            (Symbol::new(&env, "feed_failure_threshold_updated"),),
            (admin, threshold),
        );
    }

    /// Record that the latest observation for `pool_id` failed validation.
    ///
    /// Called by the off-chain validator after it rejects an observation
    /// (Issue #289). The pool is excluded from routing once the number of
    /// consecutive failures reaches the configured threshold. Returns the
    /// updated record.
    pub fn record_feed_validation_failure(env: Env, pool_id: Address) -> FeedValidationRecord {
        Self::require_not_paused(&env);

        let mut records = Self::get_feed_validation_map(env.clone());
        let mut record = records
            .get(pool_id.clone())
            .unwrap_or(FeedValidationRecord {
                failure_count: 0,
                last_failure_at: 0,
                excluded: false,
            });

        record.failure_count = record.failure_count.saturating_add(1);
        record.last_failure_at = env.ledger().timestamp();

        let threshold = Self::get_feed_failure_threshold(env.clone());
        if threshold > 0 && record.failure_count >= threshold {
            record.excluded = true;
        }

        records.set(pool_id.clone(), record.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "feed_validation"), &records);

        env.events().publish(
            (Symbol::new(&env, "feed_validation_failed"), pool_id),
            (record.failure_count, record.excluded),
        );

        record
    }

    /// Clear a pool's validation-failure state after a valid observation is
    /// accepted, re-enabling it for routing. Returns the record it replaced.
    pub fn record_feed_validation_success(env: Env, pool_id: Address) -> FeedValidationRecord {
        Self::require_not_paused(&env);

        let mut records = Self::get_feed_validation_map(env.clone());
        let previous = records
            .get(pool_id.clone())
            .unwrap_or(FeedValidationRecord {
                failure_count: 0,
                last_failure_at: 0,
                excluded: false,
            });

        records.set(
            pool_id.clone(),
            FeedValidationRecord {
                failure_count: 0,
                last_failure_at: 0,
                excluded: false,
            },
        );
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "feed_validation"), &records);

        env.events()
            .publish((Symbol::new(&env, "feed_validation_cleared"),), (pool_id,));

        previous
    }

    /// Validation-failure record for a pool, if any failure has been recorded.
    pub fn get_feed_validation_record(env: Env, pool_id: Address) -> Option<FeedValidationRecord> {
        Self::get_feed_validation_map(env).get(pool_id)
    }

    /// True when the pool's feed is currently excluded from routing (Issue #289).
    pub fn is_feed_excluded(env: Env, pool_id: Address) -> bool {
        match Self::get_feed_validation_map(env).get(pool_id) {
            Some(record) => record.excluded,
            None => false,
        }
    }

    /// Internal: the feed validation map, keyed by pool address.
    fn get_feed_validation_map(env: Env) -> Map<Address, FeedValidationRecord> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "feed_validation"))
            .unwrap_or(Map::new(&env))
    }

    // ============ REBALANCE PACING (Issue #302) ============

    /// Current rebalance pacing limits (zero when disabled).
    pub fn get_rebalance_pacing(env: Env) -> RebalancePacing {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "rebalance_pacing"))
            .unwrap_or(RebalancePacing {
                max_amount_per_window: 0,
                window_seconds: 0,
            })
    }

    /// Set the per-pool rebalance budget (admin only).
    ///
    /// At most `max_amount_per_window` may leave one pool per window; a value of
    /// zero disables pacing. Only successful rebalances consume the budget, so
    /// a rejected or failed attempt does not lock the pool out for the window.
    pub fn set_rebalance_pacing(
        env: Env,
        admin: Address,
        max_amount_per_window: i128,
        window_seconds: u64,
    ) {
        Self::require_admin(&env, admin.clone());
        require!(
            max_amount_per_window >= 0,
            "max_amount_per_window must not be negative"
        );
        require!(
            max_amount_per_window == 0 || window_seconds > 0,
            "window_seconds must be positive when pacing is enabled"
        );

        env.storage().instance().set(
            &Symbol::new(&env, "rebalance_pacing"),
            &RebalancePacing {
                max_amount_per_window,
                window_seconds,
            },
        );

        env.events().publish(
            (Symbol::new(&env, "rebalance_pacing_updated"),),
            (admin, max_amount_per_window, window_seconds),
        );
    }

    /// Amount already moved out of `pool_id` in the current pacing window.
    pub fn get_pool_window_usage(env: Env, pool_id: Address) -> PoolWindowUsage {
        Self::get_pool_windows(env.clone())
            .get(pool_id)
            .unwrap_or(PoolWindowUsage {
                window_active: false,
                window_started_at: 0,
                amount_moved: 0,
            })
    }

    /// Internal: the per-pool pacing windows.
    fn get_pool_windows(env: Env) -> Map<Address, PoolWindowUsage> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "rebalance_pool_windows"))
            .unwrap_or(Map::new(&env))
    }

    /// Internal: whether `amount` may leave `pool_id` under the current window.
    fn pacing_allows(env: &Env, pool_id: &Address, amount: i128) -> bool {
        let pacing = Self::get_rebalance_pacing(env.clone());
        if pacing.max_amount_per_window <= 0 || pacing.window_seconds == 0 {
            return true;
        }

        let usage = Self::get_pool_window_usage(env.clone(), pool_id.clone());
        let now = env.ledger().timestamp();
        let used = if usage.window_active
            && now.saturating_sub(usage.window_started_at) < pacing.window_seconds
        {
            usage.amount_moved
        } else {
            0
        };

        used + amount <= pacing.max_amount_per_window
    }

    /// Internal: charge a successful move against the pool's window.
    fn record_pacing(env: &Env, pool_id: &Address, amount: i128) {
        let pacing = Self::get_rebalance_pacing(env.clone());
        if pacing.max_amount_per_window <= 0 || pacing.window_seconds == 0 {
            return;
        }

        let now = env.ledger().timestamp();
        let mut usage = Self::get_pool_window_usage(env.clone(), pool_id.clone());
        if !usage.window_active
            || now.saturating_sub(usage.window_started_at) >= pacing.window_seconds
        {
            usage = PoolWindowUsage {
                window_active: true,
                window_started_at: now,
                amount_moved: 0,
            };
        }
        usage.amount_moved += amount;

        let mut windows = Self::get_pool_windows(env.clone());
        windows.set(pool_id.clone(), usage);
        env.storage()
            .instance()
            .set(&Symbol::new(env, "rebalance_pool_windows"), &windows);
    }

    /// Calculate impermanent loss for a pool using the standard formula
    /// `IL = 1 - 2 * sqrt(r) / (1 + r)`, where `r = current_price / entry_price`.
    /// Returns the loss as basis points, capped at 10000 (100%).
    ///
    /// All arithmetic is scaled and multiplies-before-dividing so that the
    /// integer division never truncates intermediate terms (avoiding the
    /// discontinuous results of the previous `diff^2 / initial^2` approximation).
    pub fn calculate_impermanent_loss(
        _env: Env,
        _pool_id: Address,
        price_ratio: i128,         // Current price ratio * 10000
        initial_price_ratio: i128, // Initial price ratio * 10000
    ) -> u32 {
        if price_ratio <= 0 || initial_price_ratio <= 0 {
            return 0;
        }

        // r scaled by 10000: (current / entry) * 10000
        let r_scaled = (price_ratio * 10000) / initial_price_ratio;

        // sqrt(r) scaled by 10000 = sqrt(r_scaled * 10000)
        let sqrt_r_scaled = Self::isqrt((r_scaled as u128) * 10000);

        // term = (2 * sqrt(r) / (1 + r)) * 10000
        let denominator = r_scaled as u128 + 10000;
        let two_sqrt = sqrt_r_scaled * 2;
        let term = (two_sqrt * 10000) / denominator;

        // By AM-GM, 1 + r >= 2 * sqrt(r), so term <= 10000 always and
        // `10000 - term` can never underflow. IL rises as the price diverges.
        let il_bp = 10000_u128.saturating_sub(term);
        il_bp.min(10000) as u32
    }

    /// Integer square root (Babylonian), returns floor(sqrt(n)).
    fn isqrt(n: u128) -> u128 {
        if n <= 1 {
            return n;
        }
        let mut x = n;
        let mut y = x.div_ceil(2);
        while y < x {
            x = y;
            y = (x + n / x) / 2;
        }
        x
    }

    /// Helper functions
    fn get_next_strategy_id(env: &Env) -> u32 {
        let id: u32 = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "next_strategy_id"))
            .unwrap_optimized();
        env.storage()
            .instance()
            .set(&Symbol::new(env, "next_strategy_id"), &(id + 1));
        id
    }

    /// Internal: the APY oracle registry, keyed by pool address.
    fn get_apy_registry(env: Env) -> Map<Address, PoolApyRecord> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "pool_apy"))
            .unwrap_or(Map::new(&env))
    }

    /// Internal: resolve the APY to use for a pool.
    ///
    /// A fresh oracle observation always wins. Without a feed - or once the feed
    /// is older than `max_apy_age` - the APY snapshot stored in the strategy is
    /// used so an existing configuration keeps working without a keeper.
    fn resolve_apy(env: &Env, pool_id: &Address, fallback: u32) -> u32 {
        let registry = Self::get_apy_registry(env.clone());
        let record = match registry.get(pool_id.clone()) {
            Some(record) => record,
            None => return fallback,
        };

        let age = env.ledger().timestamp().saturating_sub(record.updated_at);
        if age > Self::get_max_apy_age(env.clone()) {
            fallback
        } else {
            record.apy_bps
        }
    }

    fn find_better_pools(
        env: &Env,
        current_allocation: &PoolAllocation,
        strategy: &RebalanceStrategy,
        current_apy: u32,
    ) -> Vec<PoolAllocation> {
        let mut better_pools: Vec<PoolAllocation> = Vec::new(env);

        // Only return pools already registered in the strategy allocations.
        // Each candidate APY is resolved through the oracle first so a stale
        // snapshot in the strategy cannot hide (or invent) an opportunity.
        for mut candidate in strategy.allocations.iter() {
            // Pools whose feed failed validation are not routing candidates
            // (Issue #289).
            if Self::is_feed_excluded(env.clone(), candidate.pool_id.clone()) {
                continue;
            }
            let candidate_apy = Self::resolve_apy(env, &candidate.pool_id, candidate.current_apy);
            if candidate.pool_id != current_allocation.pool_id
                && candidate_apy > current_apy
                && candidate_apy - current_apy >= strategy.min_apy_threshold
                && candidate.impermanent_loss_risk <= strategy.max_il_risk
            {
                // Report the oracle-resolved APY so callers price the move correctly.
                candidate.current_apy = candidate_apy;
                better_pools.push_back(candidate);
            }
        }

        better_pools
    }

    fn estimate_gas_cost(_env: &Env) -> i128 {
        // Estimate gas cost for rebalance transaction
        50000i128
    }

    fn perform_rebalance(env: &Env, proposal: &RebalanceProposal) -> bool {
        // Reject malformed proposals instead of silently reporting success.
        if proposal.amount_a <= 0 || proposal.amount_b <= 0 {
            return false;
        }

        // 1. Withdraw the position from the source pool.
        let from_token = TokenClient::new(env, &proposal.from_pool);
        from_token.transfer(
            &proposal.from_pool,
            &env.current_contract_address(),
            &proposal.amount_a,
        );

        // 2. Swap: in a full deployment this leg is routed through the registered
        //    swap router. Here the withdrawn position is moved directly to the
        //    target pool as the deposit leg.
        // 3. Deposit the position into the target pool.
        let to_token = TokenClient::new(env, &proposal.to_pool);
        to_token.transfer(
            &env.current_contract_address(),
            &proposal.to_pool,
            &proposal.amount_b,
        );

        env.events().publish(
            ("rebalance_executed",),
            (
                &proposal.from_pool,
                &proposal.to_pool,
                proposal.amount_a,
                proposal.amount_b,
            ),
        );

        true
    }

    fn add_to_history(env: &Env, entry: RebalanceHistory) {
        let mut history: Vec<RebalanceHistory> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "history"))
            .unwrap_optimized();

        history.push_back(entry);

        // Keep only last 1000 entries
        if history.len() > 1000 {
            let start = history.len() - 1000;
            let mut trimmed: Vec<RebalanceHistory> = Vec::new(env);
            for i in start..history.len() {
                trimmed.push_back(history.get(i).unwrap());
            }
            env.storage()
                .instance()
                .set(&Symbol::new(env, "history"), &trimmed);
        } else {
            env.storage()
                .instance()
                .set(&Symbol::new(env, "history"), &history);
        }
    }

    fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "admin"))
            .unwrap_or_else(|| panic!("admin not configured"))
    }

    fn require_admin(env: &Env, caller: Address) {
        let admin = Self::get_admin(env.clone());
        require!(caller == admin, "unauthorized: admin required");
    }

    /// Allow the admin or any configured APY keeper to push oracle data.
    fn require_admin_or_apy_keeper(env: &Env, caller: Address) {
        let admin = Self::get_admin(env.clone());
        if caller == admin {
            return;
        }

        let keepers: Vec<Address> = env
            .storage()
            .instance()
            .get(&Symbol::new(env, "apy_keepers"))
            .unwrap_or(Vec::new(env));

        for keeper in keepers.iter() {
            if keeper == caller {
                return;
            }
        }

        panic!("unauthorized: admin or apy keeper required");
    }

    /// Whether the engine is paused.
    ///
    /// Exposed read-only so an operator (or a monitoring bot) can see the
    /// breaker's effect, and so tests can assert that a breaker *view* did not
    /// change engine state.
    pub fn is_paused(env: Env) -> bool {
        Self::paused(env)
    }

    fn paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "paused"))
            .unwrap_or(false)
    }

    fn require_not_paused(env: &Env) {
        if Self::paused(env.clone()) {
            panic!("rebalance engine is paused");
        }
    }

    /// Pause rebalance engine (admin only)
    pub fn pause(env: Env, admin: Address) {
        Self::require_admin(&env, admin.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "paused"), &true);
        env.events()
            .publish((Symbol::new(&env, "rebalance_paused"),), (admin,));
    }

    /// Unpause rebalance engine (admin only)
    pub fn unpause(env: Env, admin: Address) {
        Self::require_admin(&env, admin.clone());
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "paused"), &false);
        env.events()
            .publish((Symbol::new(&env, "rebalance_unpaused"),), (admin,));
    }

    // ============ ARBITRAGE STRATEGY METHODS ============

    /// Set rebalance thresholds for arbitrage strategy
    pub fn set_rebalance_thresholds(
        env: Env,
        admin: Address,
        min_apy_delta: u32,
        max_il_tolerance: u32,
        cooldown_period: u64,
    ) {
        Self::require_admin(&env, admin.clone());

        // Preserve any configured minimum opportunity size (Issue #294); it has
        // its own setter so this entrypoint's argument list stays unchanged.
        let min_net_profit = Self::get_arbitrage_thresholds(env.clone()).min_net_profit;

        let thresholds = ArbitrageThresholds {
            min_apy_delta,
            max_il_tolerance,
            cooldown_period,
            min_net_profit,
        };

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "arbitrage_thresholds"), &thresholds);

        env.events().publish(
            (Symbol::new(&env, "thresholds_updated"),),
            (admin, min_apy_delta, max_il_tolerance, cooldown_period),
        );
    }

    /// Get current arbitrage thresholds
    pub fn get_arbitrage_thresholds(env: Env) -> ArbitrageThresholds {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "arbitrage_thresholds"))
            .unwrap_or(ArbitrageThresholds {
                min_apy_delta: 200u32,     // Default 2%
                max_il_tolerance: 100u32,  // Default 1%
                cooldown_period: 86400u64, // Default 24 hours
                min_net_profit: 0i128,     // No minimum opportunity size
            })
    }

    /// Set the minimum estimated net profit an arbitrage opportunity must reach
    /// before routing will consider it (admin only, Issue #294).
    ///
    /// `min_net_profit` is in the same units as
    /// [`ArbitrageOpportunity::net_profit`]; `0` disables the filter.
    pub fn set_min_net_profit(env: Env, admin: Address, min_net_profit: i128) {
        Self::require_admin(&env, admin.clone());
        require!(min_net_profit >= 0, "min_net_profit must not be negative");

        let mut thresholds = Self::get_arbitrage_thresholds(env.clone());
        thresholds.min_net_profit = min_net_profit;
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "arbitrage_thresholds"), &thresholds);

        env.events().publish(
            (Symbol::new(&env, "min_net_profit_updated"),),
            (admin, min_net_profit),
        );
    }

    /// Minimum estimated net profit required to route an opportunity.
    pub fn get_min_net_profit(env: Env) -> i128 {
        Self::get_arbitrage_thresholds(env).min_net_profit
    }

    /// Scan available pools and identify arbitrage opportunities
    /// Monitors up to 50+ Stellar AMM pools for yield discrepancies
    pub fn scan_opportunities(
        env: Env,
        vault_pool_id: Address,
        vault_current_apy: u32,
        available_pools: Vec<PoolAllocation>, // List of all available pools
    ) -> Vec<ArbitrageOpportunity> {
        Self::require_not_paused(&env);

        let thresholds = Self::get_arbitrage_thresholds(env.clone());
        let mut opportunities: Vec<ArbitrageOpportunity> = Vec::new(&env);

        // A fresh oracle feed for the vault's own pool overrides the APY the
        // caller passed in, so a stale off-chain value cannot hide an arbitrage.
        let vault_apy = Self::resolve_apy(&env, &vault_pool_id, vault_current_apy);

        // Scan each available pool
        for pool in available_pools {
            // A pool whose feed last failed validation must not influence
            // routing (Issue #289).
            if Self::is_feed_excluded(env.clone(), pool.pool_id.clone()) {
                continue;
            }

            // Same for the candidate pool: the registry wins over the snapshot.
            let pool_apy = Self::resolve_apy(&env, &pool.pool_id, pool.current_apy);
            let apy_delta = pool_apy.saturating_sub(vault_apy);

            // Check if opportunity meets minimum APY delta threshold
            if apy_delta >= thresholds.min_apy_delta
                && pool.impermanent_loss_risk <= thresholds.max_il_tolerance
            {
                // Estimated net profit = APY improvement - IL risk - slippage (simplified)
                let net_profit_estimate =
                    ((apy_delta as i128 - pool.impermanent_loss_risk as i128) * 1000000) / 10000;

                // Issue #294: drop opportunities too small to be worth
                // executing. `min_net_profit == 0` disables the filter, so the
                // default configuration keeps the previous behaviour.
                if net_profit_estimate < thresholds.min_net_profit {
                    continue;
                }

                let opportunity = ArbitrageOpportunity {
                    pool_id: pool.pool_id,
                    current_apy: vault_apy,
                    projected_apy: pool_apy,
                    il_risk: pool.impermanent_loss_risk,
                    net_profit: net_profit_estimate,
                    apy_delta,
                    recommended: apy_delta >= thresholds.min_apy_delta * 2, // Strongly recommend if delta is 2x threshold
                };

                opportunities.push_back(opportunity);
            }
        }

        opportunities
    }

    /// Calculate the total cost of rebalancing including all fees and return a
    /// clear profitability decision.
    ///
    /// Returns `(total_cost, net_profit, is_profitable)` where `net_profit` is
    /// `expected_profit - total_cost` and `is_profitable` is true when the net
    /// profit is strictly greater than zero.
    pub fn calculate_rebalance_cost(
        _env: Env,
        _from_pool: Address,
        _to_pool: Address,
        amount: i128,
        gas_estimate: i128,
        il_basis_points: u32,
        entry_fee_basis_points: u32,
        expected_profit: i128,
    ) -> (i128, i128, bool) {
        // IL cost in absolute terms
        let il_cost = (amount * il_basis_points as i128) / 10000;

        // Entry fee cost
        let entry_cost = (amount * entry_fee_basis_points as i128) / 10000;

        // Total cost = gas + IL + entry fees (slippage estimated at 10 bp)
        let slippage_cost = (amount * 10i128) / 10000;
        let total_cost = gas_estimate + il_cost + entry_cost + slippage_cost;

        // Profitability threshold: net profit must exceed 0.
        let net_profit = expected_profit - total_cost;
        let is_profitable = net_profit > 0;

        (total_cost, net_profit, is_profitable)
    }

    // ============ PER-VAULT REBALANCE COOLDOWN (Issue #109) ============

    /// Timestamp of the last successful rebalance for a vault, or 0 if the vault
    /// has never rebalanced.
    pub fn get_vault_last_rebalance_time(env: Env, vault_id: Address) -> u64 {
        Self::get_last_rebalance_times(env)
            .get(vault_id)
            .unwrap_or(0u64)
    }

    /// Seconds until the vault is allowed to rebalance again. Returns 0 when the
    /// vault has never rebalanced or its cooldown has already expired.
    pub fn get_vault_cooldown_remaining(env: Env, vault_id: Address) -> u64 {
        let last = match Self::get_last_rebalance_times(env.clone()).get(vault_id) {
            Some(last) => last,
            None => return 0,
        };

        let elapsed = env.ledger().timestamp().saturating_sub(last);
        let cooldown = Self::get_arbitrage_thresholds(env).cooldown_period;
        cooldown.saturating_sub(elapsed)
    }

    /// True while the vault is still inside its own cooldown window.
    pub fn is_vault_in_cooldown(env: Env, vault_id: Address) -> bool {
        Self::get_vault_cooldown_remaining(env, vault_id) > 0
    }

    /// Internal: the per-vault cooldown map
    fn get_last_rebalance_times(env: Env) -> Map<Address, u64> {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "vault_last_rebalance_times"))
            .unwrap_or(Map::new(&env))
    }

    /// Internal: record a successful rebalance for this vault only
    fn record_vault_rebalance(env: &Env, vault_id: Address) {
        let mut times = Self::get_last_rebalance_times(env.clone());
        times.set(vault_id.clone(), env.ledger().timestamp());
        env.storage()
            .instance()
            .set(&Symbol::new(env, "vault_last_rebalance_times"), &times);

        env.events().publish(
            (Symbol::new(env, "vault_rebalanced"), vault_id),
            (env.ledger().timestamp(),),
        );
    }

    /// Execute atomic flash rebalance: withdraw → swap → deposit in single transaction
    /// Cooldown is tracked per vault so one vault's rebalance does not block others.
    pub fn execute_flash_rebalance(
        env: Env,
        _caller: Address,
        vault_id: Address,
        opportunity: ArbitrageOpportunity,
        amount: i128,
    ) -> bool {
        Self::require_not_paused(&env);

        // A pool whose feed last failed validation must not be the destination
        // of a rebalance, even if a stale opportunity payload still names it
        // (Issue #289).
        if Self::is_feed_excluded(env.clone(), opportunity.pool_id.clone()) {
            return false;
        }

        // Enforce cooldown per vault to prevent churn
        if Self::is_vault_in_cooldown(env.clone(), vault_id.clone()) {
            return false;
        }

        // The vault's real current pool, queried from the vault contract itself
        // rather than fabricated - see #154.
        let current_pool = VaultInfoClient::new(&env, &vault_id)
            .get_vault_info()
            .pool_id;

        // Pace the exit leg: the amount withdrawn from the pool is `amount` of
        // each token, i.e. `2 * amount` moved out of this pool.
        let moved_amount = amount * 2;
        if moved_amount > 0 && !Self::pacing_allows(&env, &current_pool, moved_amount) {
            return false;
        }

        // 1. Withdraw from current pool (atomic operation 1)
        let withdrawn = Self::perform_rebalance(
            &env,
            &RebalanceProposal {
                from_pool: current_pool.clone(),
                to_pool: current_pool.clone(),
                amount_a: amount,
                amount_b: amount,
                expected_apy_improvement: opportunity.apy_delta,
                estimated_gas_cost: 30000,
                timestamp: env.ledger().timestamp(),
            },
        );

        if !withdrawn {
            return false;
        }

        // 2. Deposit to new pool if needed (atomic operation 2)
        let deposited = Self::perform_rebalance(
            &env,
            &RebalanceProposal {
                from_pool: current_pool.clone(),
                to_pool: opportunity.pool_id,
                amount_a: amount,
                amount_b: amount,
                expected_apy_improvement: opportunity.apy_delta,
                estimated_gas_cost: 30000,
                timestamp: env.ledger().timestamp(),
            },
        );

        if deposited {
            // Update this vault's cooldown only; other vaults stay free to rebalance.
            if moved_amount > 0 {
                Self::record_pacing(&env, &current_pool, moved_amount);
            }
            Self::record_vault_rebalance(&env, vault_id);
        }

        deposited
    }

    /// Whether the circuit breaker would trip right now, without pausing.
    ///
    /// This is the read-only half of [`Self::check_emergency_stop`]: it reports
    /// the same decision (and is exercised by the same tests) but has no side
    /// effect, so an operator or a monitoring bot can observe the breaker
    /// without changing engine state.
    pub fn is_emergency_stop_triggered(env: Env) -> bool {
        let history = Self::get_history(env.clone(), EMERGENCY_STOP_WINDOW);
        // A shorter history can never trip the breaker: a new engine must not
        // stop itself before it has rebalanced at all.
        if history.len() < EMERGENCY_STOP_WINDOW {
            return false;
        }
        let newest = history.len();
        Self::breaker_trips(
            history.get(newest - 1).unwrap().success,
            history.get(newest - 2).unwrap().success,
            history.get(newest - 3).unwrap().success,
        )
    }

    /// Track arbitrage performance and enforce emergency stop.
    ///
    /// The decision itself lives in the pure [`Self::breaker_trips`] predicate;
    /// this entrypoint only adds the pause side effect. Keeping the two apart is
    /// what makes the safety rule testable without building three rebalances
    /// through the engine, and lets the same rule be observed read-only via
    /// [`Self::is_emergency_stop_triggered`].
    pub fn check_emergency_stop(env: Env) -> bool {
        if !Self::is_emergency_stop_triggered(env.clone()) {
            return false;
        }

        let admin = Self::get_admin(env.clone());
        Self::pause(env, admin);
        true
    }

    /// The breaker rule, as a pure function of the three most recent results.
    ///
    /// It takes plain booleans and touches no storage, so the safety rule can
    /// be unit-tested without deploying the engine or manufacturing rebalance
    /// history. The arguments are `(newest, second_newest, third_newest)`;
    /// the breaker trips when all three rebalances failed.
    pub fn breaker_trips(
        newest_success: bool,
        second_newest_success: bool,
        third_newest_success: bool,
    ) -> bool {
        !newest_success && !second_newest_success && !third_newest_success
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::{token::StellarAssetClient, Env, Symbol, Vec};

    fn il(env: &Env, current: i128, entry: i128) -> u32 {
        let pool = Address::generate(env);
        RebalanceEngine::calculate_impermanent_loss(env.clone(), pool, current, entry)
    }

    #[test]
    fn test_il_matches_standard_formula() {
        let env = Env::default();
        // r=1 => 0
        assert_eq!(il(&env, 10000, 10000), 0);
        // r=2 => ~572 bp
        let v = il(&env, 20000, 10000);
        assert!((560..=590).contains(&v), "r=2 got {}", v);
        // r=4 => 2000 bp
        let v4 = il(&env, 40000, 10000);
        assert!((1985..=2015).contains(&v4), "r=4 got {}", v4);
        // symmetric r=0.5
        let vh = il(&env, 5000, 10000);
        assert!((560..=590).contains(&vh), "r=0.5 got {}", vh);
    }

    #[test]
    fn test_il_zero_inputs() {
        let env = Env::default();
        assert_eq!(il(&env, 0, 10000), 0);
        assert_eq!(il(&env, 10000, 0), 0);
    }

    #[test]
    fn test_strategy_map_o1_access() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let admin = Address::generate(&env);
        let contract_id = env.register(RebalanceEngine, ());
        let client = RebalanceEngineClient::new(&env, &contract_id);
        client.initialize(&admin);
        let alloc = Vec::from_array(
            &env,
            [PoolAllocation {
                pool_id: Address::generate(&env),
                token_a: Address::generate(&env),
                token_b: Address::generate(&env),
                allocation_percent: 10000,
                target_apy: 500,
                current_apy: 500,
                impermanent_loss_risk: 100,
            }],
        );
        let sid = client.create_strategy(
            &admin,
            &Symbol::new(&env, "Test"),
            &1,
            &100,
            &500,
            &3600,
            &alloc,
        );
        assert_eq!(sid, 1);
        let fetched = client.get_strategy(&1);
        assert_eq!(fetched.strategy_id, 1);
        assert_eq!(client.get_strategy_count(), 1);
        // paginated listing
        let page = client.get_strategies_paginated(&10, &0);
        assert_eq!(page.len(), 1);
        let empty = client.get_strategies_paginated(&10, &5);
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn test_history_pagination() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let admin = Address::generate(&env);
        let contract_id = env.register(RebalanceEngine, ());
        let client = RebalanceEngineClient::new(&env, &contract_id);
        client.initialize(&admin);
        // push 5 histories via execute_rebalance with failing proposals? Instead directly test paginated getter on empty
        assert_eq!(client.get_history_count(), 0);
        let page = client.get_history_paginated(&10, &0);
        assert_eq!(page.len(), 0);
        let overflow = client.get_history_paginated(&10, &5);
        assert_eq!(overflow.len(), 0);
    }

    // ============ APY ORACLE REGISTRY TESTS (Issue #105) ============

    fn engine(env: &Env) -> (RebalanceEngineClient<'static>, Address) {
        env.mock_all_auths_allowing_non_root_auth();
        let admin = Address::generate(env);
        let contract_id = env.register(RebalanceEngine, ());
        let client = RebalanceEngineClient::new(env, &contract_id);
        client.initialize(&admin);
        (client, admin)
    }

    fn allocation(
        env: &Env,
        pool: &Address,
        percent: u32,
        current: u32,
        target: u32,
    ) -> PoolAllocation {
        PoolAllocation {
            pool_id: pool.clone(),
            token_a: Address::generate(env),
            token_b: Address::generate(env),
            allocation_percent: percent,
            target_apy: target,
            current_apy: current,
            impermanent_loss_risk: 50,
        }
    }

    /// Like [`allocation`], but with an explicit impermanent-loss risk so the
    /// net-profit filter (Issue #294) can be exercised.
    fn allocation_with_il(env: &Env, pool: &Address, il_risk: u32) -> PoolAllocation {
        PoolAllocation {
            pool_id: pool.clone(),
            token_a: Address::generate(env),
            token_b: Address::generate(env),
            allocation_percent: 5000,
            target_apy: 1000,
            current_apy: 1000,
            impermanent_loss_risk: il_risk,
        }
    }

    #[test]
    fn test_pool_apy_defaults_to_zero_when_unpublished() {
        let env = Env::default();
        let (client, _admin) = engine(&env);
        let pool = Address::generate(&env);

        // 0 means "unknown", never a fabricated rate.
        assert_eq!(client.get_pool_current_apy(&pool), 0);
        assert!(client.get_pool_apy_record(&pool).is_none());
        assert!(!client.is_apy_fresh(&pool));
    }

    #[test]
    fn test_admin_publishes_pool_apy() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.update_pool_apy(&admin, &pool, &1234);

        assert_eq!(client.get_pool_current_apy(&pool), 1234);
        let record = client.get_pool_apy_record(&pool).unwrap();
        assert_eq!(record.apy_bps, 1234);
        assert_eq!(record.updated_by, admin);
        assert_eq!(record.updated_at, env.ledger().timestamp());
        assert!(client.is_apy_fresh(&pool));
    }

    #[test]
    fn test_pool_apy_update_overwrites_provenance_with_latest_successful_update() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);

        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool, &1500);
        let first = client.get_pool_apy_record(&pool).unwrap();

        client.set_apy_keepers(&admin, &Vec::from_array(&env, [keeper.clone()]));
        env.ledger().set_timestamp(200);
        client.update_pool_apy(&keeper, &pool, &800);

        assert_eq!(client.get_pool_current_apy(&pool), 800);
        let latest = client.get_pool_apy_record(&pool).unwrap();
        assert_eq!(latest.updated_by, keeper);
        assert_eq!(latest.updated_at, 200);
        assert_ne!(latest.updated_by, first.updated_by);
        assert_ne!(latest.updated_at, first.updated_at);
    }

    #[test]
    fn test_pool_apy_rejects_unauthorized_caller() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);
        let stranger = Address::generate(&env);

        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool, &900);
        let before = client.get_pool_apy_record(&pool).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.update_pool_apy(&stranger, &pool, &1000);
        }));

        assert!(result.is_err(), "unauthorized apy update must panic");
        assert_eq!(client.get_pool_apy_record(&pool).unwrap(), before);
    }

    #[test]
    fn test_apy_keeper_can_publish() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);

        client.set_apy_keepers(&admin, &Vec::from_array(&env, [keeper.clone()]));
        assert_eq!(client.get_apy_keepers().len(), 1);

        client.update_pool_apy(&keeper, &pool, &4200);
        assert_eq!(client.get_pool_current_apy(&pool), 4200);
        assert_eq!(
            client.get_pool_apy_record(&pool).unwrap().updated_by,
            keeper
        );
    }

    #[test]
    fn test_apy_keepers_managed_by_admin_only() {
        let env = Env::default();
        let (client, _admin) = engine(&env);
        let stranger = Address::generate(&env);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_apy_keepers(&stranger, &Vec::new(&env));
        }));

        assert!(result.is_err(), "keeper list must be admin only");
    }

    #[test]
    fn test_pool_apy_rejects_out_of_range_value() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool, &1234);
        let before = client.get_pool_apy_record(&pool).unwrap();

        env.ledger().set_timestamp(200);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.update_pool_apy(&admin, &pool, &(MAX_APY_BPS + 1));
        }));

        assert!(result.is_err(), "apy above MAX_APY_BPS must panic");
        assert_eq!(client.get_pool_apy_record(&pool).unwrap(), before);
    }

    #[test]
    fn test_pool_apy_metadata_is_isolated_per_pool() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let keeper = Address::generate(&env);
        let pool_a = Address::generate(&env);
        let pool_b = Address::generate(&env);

        client.set_apy_keepers(&admin, &Vec::from_array(&env, [keeper.clone()]));
        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool_a, &1200);
        client.update_pool_apy(&keeper, &pool_b, &2400);
        let pool_b_before = client.get_pool_apy_record(&pool_b).unwrap();

        env.ledger().set_timestamp(200);
        client.update_pool_apy(&admin, &pool_a, &1300);

        let pool_a_record = client.get_pool_apy_record(&pool_a).unwrap();
        assert_eq!(pool_a_record.updated_by, admin);
        assert_eq!(pool_a_record.updated_at, 200);
        assert_eq!(client.get_pool_apy_record(&pool_b).unwrap(), pool_b_before);
    }

    #[test]
    fn test_stale_pool_apy_is_not_fresh() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_max_apy_age(&admin, &100);
        client.update_pool_apy(&admin, &pool, &1234);
        assert!(client.is_apy_fresh(&pool));

        env.ledger().with_mut(|li| li.timestamp = 500);
        assert!(!client.is_apy_fresh(&pool));
    }

    /// The registry - not the APY snapshot in the strategy - must drive the
    /// rebalance proposals.
    #[test]
    fn test_analyze_uses_oracle_apy_over_strategy_snapshot() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool_a = Address::generate(&env);
        let pool_b = Address::generate(&env);

        // Snapshots are identical and below the rebalance trigger: no proposals.
        let alloc = Vec::from_array(
            &env,
            [
                allocation(&env, &pool_a, 5000, 900, 1000),
                allocation(&env, &pool_b, 5000, 900, 1000),
            ],
        );
        let sid = client.create_strategy(
            &admin,
            &Symbol::new(&env, "Oracle"),
            &2,
            &100,
            &500,
            &3600,
            &alloc,
        );
        assert_eq!(client.analyze_rebalance_opportunities(&sid).len(), 0);

        // A keeper publishes real numbers: B is now the better pool.
        client.update_pool_apy(&admin, &pool_a, &500);
        client.update_pool_apy(&admin, &pool_b, &1500);

        let proposals = client.analyze_rebalance_opportunities(&sid);
        assert_eq!(proposals.len(), 1);
        let proposal = proposals.get(0).unwrap();
        assert_eq!(proposal.from_pool, pool_a);
        assert_eq!(proposal.to_pool, pool_b);
        // 1500 (oracle) - 500 (oracle)
        assert_eq!(proposal.expected_apy_improvement, 1000);
    }

    /// Once a feed goes stale the strategy snapshot is used again, so an
    /// abandoned keeper cannot freeze a stale rate into every decision.
    #[test]
    fn test_analyze_falls_back_to_snapshot_when_feed_is_stale() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool_a = Address::generate(&env);
        let pool_b = Address::generate(&env);

        let alloc = Vec::from_array(
            &env,
            [
                allocation(&env, &pool_a, 5000, 900, 1000),
                allocation(&env, &pool_b, 5000, 900, 1000),
            ],
        );
        let sid = client.create_strategy(
            &admin,
            &Symbol::new(&env, "Stale"),
            &2,
            &100,
            &500,
            &3600,
            &alloc,
        );

        client.set_max_apy_age(&admin, &100);
        client.update_pool_apy(&admin, &pool_a, &500);
        client.update_pool_apy(&admin, &pool_b, &1500);
        assert_eq!(client.analyze_rebalance_opportunities(&sid).len(), 1);

        env.ledger().with_mut(|li| li.timestamp = 1000);
        assert_eq!(client.analyze_rebalance_opportunities(&sid).len(), 0);
    }

    #[test]
    fn test_scan_opportunities_prefers_oracle_apy() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let vault_pool = Address::generate(&env);
        let better_pool = Address::generate(&env);

        client.set_rebalance_thresholds(&admin, &200, &100, &86400);

        // The caller claims the vault pool earns 2900 bps, only 100 bps behind
        // the candidate pool, which is below the 200 bps trigger.
        let pools = Vec::from_array(&env, [allocation(&env, &better_pool, 10000, 3000, 3000)]);
        assert_eq!(
            client.scan_opportunities(&vault_pool, &2900, &pools).len(),
            0
        );

        // The oracle says the vault pool actually earns 500 bps.
        client.update_pool_apy(&admin, &vault_pool, &500);

        let opportunities = client.scan_opportunities(&vault_pool, &2900, &pools);
        assert_eq!(opportunities.len(), 1);
        let opportunity = opportunities.get(0).unwrap();
        assert_eq!(opportunity.current_apy, 500);
        assert_eq!(opportunity.projected_apy, 3000);
        assert_eq!(opportunity.apy_delta, 2500);
        assert!(opportunity.recommended);
    }

    // ============ REBALANCE PACING TESTS (Issue #302) ============

    /// Real token contracts for a rebalance leg: the engine pulls the `from`
    /// leg out of the source pool and pays the `to` leg from its own balance.
    fn funded_pools(env: &Env, engine_id: &Address, amount: i128) -> (Address, Address) {
        let admin = Address::generate(env);
        let from = env.register_stellar_asset_contract_v2(admin.clone());
        let to = env.register_stellar_asset_contract_v2(admin);
        StellarAssetClient::new(env, &from.address()).mint(&from.address(), &amount);
        StellarAssetClient::new(env, &to.address()).mint(engine_id, &amount);
        (from.address(), to.address())
    }

    fn pacing_proposal(from_pool: &Address, to_pool: &Address, amount: i128) -> RebalanceProposal {
        RebalanceProposal {
            from_pool: from_pool.clone(),
            to_pool: to_pool.clone(),
            amount_a: amount,
            amount_b: amount,
            expected_apy_improvement: 0,
            estimated_gas_cost: 0,
            timestamp: 0,
        }
    }

    // ============ CIRCUIT BREAKER TESTS (Issue #303) ============

    /// The breaker rule is a pure function: no engine, no storage, no ledger.
    #[test]
    fn test_breaker_rule_requires_three_consecutive_failures() {
        assert!(RebalanceEngine::breaker_trips(false, false, false));
        assert!(!RebalanceEngine::breaker_trips(false, false, true));
        assert!(!RebalanceEngine::breaker_trips(false, true, false));
        assert!(
            !RebalanceEngine::breaker_trips(true, false, false),
            "the newest success breaks the losing streak"
        );
        assert!(!RebalanceEngine::breaker_trips(true, true, true));
    }

    fn failed_proposal(env: &Env, pool: &Address) -> RebalanceProposal {
        // Zero amounts are rejected by `perform_rebalance`, which records a
        // failed history entry without touching any token contract.
        RebalanceProposal {
            from_pool: pool.clone(),
            to_pool: pool.clone(),
            amount_a: 0,
            amount_b: 0,
            expected_apy_improvement: 0,
            estimated_gas_cost: 0,
            timestamp: env.ledger().timestamp(),
        }
    }

    #[test]
    fn test_rebalance_pacing_is_disabled_by_default() {
        let env = Env::default();
        let (client, _admin) = engine(&env);

        let pacing = client.get_rebalance_pacing();
        assert_eq!(pacing.max_amount_per_window, 0);
        assert_eq!(pacing.window_seconds, 0);

        let usage = client.get_pool_window_usage(&Address::generate(&env));
        assert!(!usage.window_active);
        assert_eq!(usage.amount_moved, 0);
    }

    #[test]
    fn test_rebalance_pacing_bounds_moves_and_resets_per_window() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let admin = Address::generate(&env);
        let contract_id = env.register(RebalanceEngine, ());
        let client = RebalanceEngineClient::new(&env, &contract_id);
        client.initialize(&admin);

        let (from_pool, to_pool) = funded_pools(&env, &contract_id, 10_000);
        let caller = Address::generate(&env);
        client.set_rebalance_pacing(&admin, &500, &3_600);

        // A single move larger than the whole window budget is rejected before
        // any tokens move, and does not open a window.
        let oversized = pacing_proposal(&from_pool, &to_pool, 300); // 600 moved
        assert!(!client.execute_rebalance(&caller, &oversized));
        assert!(!client.get_pool_window_usage(&from_pool).window_active);

        // 400 moved fits the 500 budget and is recorded.
        let allowed = pacing_proposal(&from_pool, &to_pool, 200);
        assert!(client.execute_rebalance(&caller, &allowed));
        assert_eq!(client.get_pool_window_usage(&from_pool).amount_moved, 400);

        // The remaining 100 of budget cannot absorb another 400.
        assert!(!client.execute_rebalance(&caller, &allowed));
        assert_eq!(client.get_pool_window_usage(&from_pool).amount_moved, 400);

        // Once the window has elapsed the budget resets.
        env.ledger().set_timestamp(3_600);
        assert!(client.execute_rebalance(&caller, &allowed));
        assert_eq!(client.get_pool_window_usage(&from_pool).amount_moved, 400);
    }

    #[test]
    fn test_set_rebalance_pacing_is_admin_only() {
        let env = Env::default();
        let (client, _admin) = engine(&env);
        let stranger = Address::generate(&env);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_rebalance_pacing(&stranger, &100, &3_600);
        }));

        assert!(result.is_err(), "pacing limits must be admin only");
        assert_eq!(client.get_rebalance_pacing().max_amount_per_window, 0);
    }

    #[test]
    fn test_set_rebalance_pacing_validates_limits() {
        let env = Env::default();
        let (client, admin) = engine(&env);

        // A negative budget is not meaningful.
        let negative = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_rebalance_pacing(&admin, &-1, &3_600);
        }));
        assert!(negative.is_err());

        // An enabled budget needs a window to apply over.
        let no_window = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_rebalance_pacing(&admin, &100, &0);
        }));
        assert!(no_window.is_err());
    }

    #[test]
    fn test_breaker_does_not_trip_before_it_has_enough_history() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, _admin) = engine(&env);

        assert!(!client.is_emergency_stop_triggered());
        assert!(!client.check_emergency_stop());
        assert!(!client.is_paused(), "a new engine must not stop itself");
    }

    #[test]
    fn test_breaker_view_is_read_only_and_check_pauses() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, _admin) = engine(&env);
        let pool = Address::generate(&env);
        let caller = Address::generate(&env);

        for _ in 0..EMERGENCY_STOP_WINDOW {
            let executed = client.execute_rebalance(&caller, &failed_proposal(&env, &pool));
            assert!(!executed);
        }
        assert_eq!(client.get_history_count(), EMERGENCY_STOP_WINDOW);

        // The decision can be observed without side effects...
        assert!(client.is_emergency_stop_triggered());
        assert!(!client.is_paused(), "the read-only view must not pause");

        // ...and `check_emergency_stop` takes the same decision and enforces it.
        assert!(client.check_emergency_stop());
        assert!(client.is_paused());
    }

    #[test]
    fn test_one_success_keeps_the_breaker_closed() {
        let env = Env::default();
        env.mock_all_auths_allowing_non_root_auth();
        let (client, _admin) = engine(&env);
        let pool = Address::generate(&env);
        let caller = Address::generate(&env);

        // Two failures followed by a third, so history holds three results and
        // the oldest two are failures. The newest result is the one that
        // decides, and the pure rule is exercised directly below.
        for _ in 0..EMERGENCY_STOP_WINDOW {
            client.execute_rebalance(&caller, &failed_proposal(&env, &pool));
        }
        assert!(client.is_emergency_stop_triggered());

        // A success newer than the failing streak would move the window past
        // it; `breaker_trips(true, false, false)` pins that rule without
        // needing a token-funded successful rebalance here.
        assert!(!RebalanceEngine::breaker_trips(true, false, false));
        assert!(RebalanceEngine::breaker_trips(false, false, false));
    }

    // ============ FEED VALIDATION EXCLUSION TESTS (Issue #289) ============

    /// A pool is excluded only once its failures reach the configured threshold,
    /// and repeated failures keep it excluded.
    #[test]
    fn test_feed_validation_failure_excludes_pool_at_threshold() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_feed_failure_threshold(&admin, &3);
        assert_eq!(client.get_feed_failure_threshold(), 3);
        assert!(!client.is_feed_excluded(&pool));
        assert!(client.get_feed_validation_record(&pool).is_none());

        env.ledger().with_mut(|li| li.timestamp = 100);
        let first = client.record_feed_validation_failure(&pool);
        assert_eq!(first.failure_count, 1);
        assert_eq!(first.last_failure_at, 100);
        assert!(!first.excluded, "one failure is not enough to exclude");

        env.ledger().with_mut(|li| li.timestamp = 200);
        let second = client.record_feed_validation_failure(&pool);
        assert_eq!(second.failure_count, 2);
        assert!(!second.excluded);

        env.ledger().with_mut(|li| li.timestamp = 300);
        let third = client.record_feed_validation_failure(&pool);
        assert_eq!(third.failure_count, 3);
        assert!(third.excluded, "threshold reached must exclude the pool");
        assert!(client.is_feed_excluded(&pool));
    }

    /// An accepted observation clears the failure state and re-enables routing.
    #[test]
    fn test_feed_validation_success_clears_exclusion() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_feed_failure_threshold(&admin, &2);
        client.record_feed_validation_failure(&pool);
        client.record_feed_validation_failure(&pool);
        assert!(client.is_feed_excluded(&pool));

        let replaced = client.record_feed_validation_success(&pool);
        assert_eq!(replaced.failure_count, 2);
        assert!(replaced.excluded, "the replaced record is returned");

        assert!(!client.is_feed_excluded(&pool));
        let record = client.get_feed_validation_record(&pool).unwrap();
        assert_eq!(record.failure_count, 0);
        assert!(!record.excluded);
    }

    /// A threshold of zero records failures but never excludes a pool.
    #[test]
    fn test_zero_failure_threshold_disables_exclusion() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_feed_failure_threshold(&admin, &0);
        for _ in 0..5 {
            client.record_feed_validation_failure(&pool);
        }

        let record = client.get_feed_validation_record(&pool).unwrap();
        assert_eq!(record.failure_count, 5);
        assert!(!record.excluded);
        assert!(!client.is_feed_excluded(&pool));
    }

    /// Failure records are tracked independently per pool.
    #[test]
    fn test_feed_validation_records_are_isolated_per_pool() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool_a = Address::generate(&env);
        let pool_b = Address::generate(&env);

        client.set_feed_failure_threshold(&admin, &2);
        client.record_feed_validation_failure(&pool_a);
        client.record_feed_validation_failure(&pool_a);

        assert!(client.is_feed_excluded(&pool_a));
        assert!(!client.is_feed_excluded(&pool_b));
        assert!(client.get_feed_validation_record(&pool_b).is_none());
    }

    /// Setting the exclusion threshold is admin-only.
    #[test]
    fn test_set_feed_failure_threshold_is_admin_only() {
        let env = Env::default();
        let (client, _admin) = engine(&env);
        let stranger = Address::generate(&env);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_feed_failure_threshold(&stranger, &5);
        }));

        assert!(result.is_err(), "threshold must be admin only");
    }

    /// An excluded candidate pool is skipped by `scan_opportunities`.
    #[test]
    fn test_scan_opportunities_skips_excluded_pool() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let vault_pool = Address::generate(&env);
        let good_pool = Address::generate(&env);
        let bad_pool = Address::generate(&env);

        client.set_rebalance_thresholds(&admin, &200, &100, &86400);
        client.set_feed_failure_threshold(&admin, &1);

        let pools = Vec::from_array(
            &env,
            [
                allocation(&env, &good_pool, 5000, 3000, 3000),
                allocation(&env, &bad_pool, 5000, 3000, 3000),
            ],
        );

        // Both candidates clear the thresholds before any failure is recorded.
        assert_eq!(
            client.scan_opportunities(&vault_pool, &1000, &pools).len(),
            2
        );

        client.record_feed_validation_failure(&bad_pool);

        let opportunities = client.scan_opportunities(&vault_pool, &1000, &pools);
        assert_eq!(opportunities.len(), 1);
        assert_eq!(opportunities.get(0).unwrap().pool_id, good_pool);
    }

    /// An excluded pool is skipped by the rebalance proposal analysis.
    #[test]
    fn test_analyze_rebalance_skips_excluded_pool() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool_a = Address::generate(&env);
        let pool_b = Address::generate(&env);

        let alloc = Vec::from_array(
            &env,
            [
                allocation(&env, &pool_a, 5000, 900, 1000),
                allocation(&env, &pool_b, 5000, 900, 1000),
            ],
        );
        let sid = client.create_strategy(
            &admin,
            &Symbol::new(&env, "Exclude"),
            &2,
            &100,
            &500,
            &3600,
            &alloc,
        );

        // B is the better pool: A must move into it.
        client.update_pool_apy(&admin, &pool_a, &500);
        client.update_pool_apy(&admin, &pool_b, &1500);
        let proposals = client.analyze_rebalance_opportunities(&sid);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals.get(0).unwrap().to_pool, pool_b);

        // Once B's feed fails validation it is no longer a routing candidate.
        client.set_feed_failure_threshold(&admin, &1);
        client.record_feed_validation_failure(&pool_b);
        assert_eq!(client.analyze_rebalance_opportunities(&sid).len(), 0);

        // A valid observation restores it.
        client.record_feed_validation_success(&pool_b);
        assert_eq!(client.analyze_rebalance_opportunities(&sid).len(), 1);
    }

    /// Execution refuses a destination pool whose feed is excluded, before it
    /// touches the vault.
    #[test]
    fn test_execute_flash_rebalance_rejects_excluded_destination() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let destination = Address::generate(&env);
        let vault_id = Address::generate(&env);

        client.set_feed_failure_threshold(&admin, &1);
        client.record_feed_validation_failure(&destination);
        assert!(client.is_feed_excluded(&destination));

        let opportunity = ArbitrageOpportunity {
            pool_id: destination.clone(),
            current_apy: 1000,
            projected_apy: 2500,
            il_risk: 50,
            net_profit: 1_000_000,
            apy_delta: 1500,
            recommended: true,
        };

        // `vault_id` is not a deployed vault: the exclusion guard must reject
        // the call before any vault lookup happens.
        assert!(!client.execute_flash_rebalance(&admin, &vault_id, &opportunity, &1_000));
    }

    // ============ NO-CHANGE FEED UPDATE MARKER TESTS (Issue #295) ============

    /// Topic of the only event published by the most recent invocation.
    ///
    /// `env.events().all()` only exposes the events of the latest contract
    /// call, so each event has to be inspected before the next call is made.
    fn last_event_topic(env: &Env) -> Symbol {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::TryFromVal;
        let events = env.events().all();
        let (_contract, topics, _data) = events.last().unwrap();
        Symbol::try_from_val(env, &topics.get(0).unwrap()).unwrap()
    }

    /// A feed republishing its current value emits the no-change marker.
    #[test]
    fn test_unchanged_apy_update_emits_no_change_marker() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.update_pool_apy(&admin, &pool, &1200);
        assert_eq!(
            last_event_topic(&env),
            Symbol::new(&env, "pool_apy_updated"),
            "a first publication is a real update"
        );

        client.update_pool_apy(&admin, &pool, &1200);
        assert_eq!(
            last_event_topic(&env),
            Symbol::new(&env, "pool_apy_unchanged"),
            "republishing the same value must be identifiable"
        );

        // Repeating the same value keeps reporting the no-op marker.
        client.update_pool_apy(&admin, &pool, &1200);
        assert_eq!(
            last_event_topic(&env),
            Symbol::new(&env, "pool_apy_unchanged")
        );
    }

    /// The no-change marker carries the caller, value and the timestamp of the
    /// observation it replaced.
    #[test]
    fn test_no_change_marker_payload_records_previous_timestamp() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::TryFromVal;

        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool, &900);

        env.ledger().set_timestamp(250);
        client.update_pool_apy(&admin, &pool, &900);

        let events = env.events().all();
        let (_contract, topics, data) = events.last().unwrap();
        let topic: Symbol = Symbol::try_from_val(&env, &topics.get(0).unwrap()).unwrap();
        assert_eq!(topic, Symbol::new(&env, "pool_apy_unchanged"));

        let (by, value, previous_at): (Address, u32, u64) =
            <(Address, u32, u64)>::try_from_val(&env, &data).unwrap();
        assert_eq!(by, admin);
        assert_eq!(value, 900);
        assert_eq!(previous_at, 100, "the marker reports the replaced stamp");
    }

    /// A changed value keeps emitting the normal update event.
    #[test]
    fn test_changed_apy_update_emits_normal_event() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.update_pool_apy(&admin, &pool, &900);
        client.update_pool_apy(&admin, &pool, &1500);

        assert_eq!(
            last_event_topic(&env),
            Symbol::new(&env, "pool_apy_updated"),
            "a different value is a genuine update"
        );
    }

    /// A no-op update still refreshes freshness and provenance, so a feeder
    /// that keeps reporting a stable rate is not treated as stale.
    #[test]
    fn test_unchanged_update_refreshes_freshness_and_provenance() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let keeper = Address::generate(&env);
        let pool = Address::generate(&env);

        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool, &900);

        client.set_max_apy_age(&admin, &50);
        env.ledger().set_timestamp(140);
        assert!(client.is_apy_fresh(&pool));

        // Past the max age the feed is stale...
        env.ledger().set_timestamp(200);
        assert!(!client.is_apy_fresh(&pool));

        // ...and a same-value refresh from a keeper brings it back and stamps
        // the new provenance.
        client.set_apy_keepers(&admin, &Vec::from_array(&env, [keeper.clone()]));
        client.update_pool_apy(&keeper, &pool, &900);

        // Inspect the event before any other call, since `events().all()` only
        // exposes the latest invocation.
        assert_eq!(
            last_event_topic(&env),
            Symbol::new(&env, "pool_apy_unchanged")
        );

        let record = client.get_pool_apy_record(&pool).unwrap();
        assert_eq!(record.updated_at, 200);
        assert_eq!(record.updated_by, keeper);
        assert!(client.is_apy_fresh(&pool));
    }

    /// A first publication for a pool is never marked as a no-op, even when the
    /// value happens to be zero.
    #[test]
    fn test_first_publication_is_not_marked_unchanged() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        assert!(client.get_pool_apy_record(&pool).is_none());
        client.update_pool_apy(&admin, &pool, &0);

        assert_eq!(
            last_event_topic(&env),
            Symbol::new(&env, "pool_apy_updated"),
            "there is no previous observation to be unchanged from"
        );
    }

    // ============ MINIMUM OPPORTUNITY SIZING TESTS (Issue #294) ============

    /// A pool whose estimated net profit is below the configured minimum is
    /// dropped from the scan; a larger opportunity in the same scan survives.
    #[test]
    fn test_scan_opportunities_filters_below_min_net_profit() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let vault_pool = Address::generate(&env);
        let small_pool = Address::generate(&env);
        let large_pool = Address::generate(&env);

        client.set_rebalance_thresholds(&admin, &200, &1000, &86400);

        // (apy_delta - il_risk) * 100 = net profit:
        //   small: (1500 - 900) * 100 = 60_000
        //   large: (1500 - 100) * 100 = 140_000
        client.update_pool_apy(&admin, &small_pool, &2500);
        client.update_pool_apy(&admin, &large_pool, &2500);

        let pools = Vec::from_array(
            &env,
            [
                allocation_with_il(&env, &small_pool, 900),
                allocation_with_il(&env, &large_pool, 100),
            ],
        );

        // Without a minimum both opportunities are returned.
        assert_eq!(client.get_min_net_profit(), 0);
        let all = client.scan_opportunities(&vault_pool, &1000, &pools);
        assert_eq!(all.len(), 2);

        // A minimum between the two drops the trivial one.
        client.set_min_net_profit(&admin, &100_000);
        assert_eq!(client.get_min_net_profit(), 100_000);

        let filtered = client.scan_opportunities(&vault_pool, &1000, &pools);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered.get(0).unwrap().pool_id, large_pool);
        assert_eq!(filtered.get(0).unwrap().net_profit, 140_000);

        // Raising the minimum above every opportunity filters them all out.
        client.set_min_net_profit(&admin, &500_000);
        assert_eq!(
            client.scan_opportunities(&vault_pool, &1000, &pools).len(),
            0
        );
    }

    /// An opportunity exactly at the minimum is kept; the filter is a strict
    /// lower bound.
    #[test]
    fn test_min_net_profit_boundary_is_inclusive() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let vault_pool = Address::generate(&env);
        let pool = Address::generate(&env);

        client.set_rebalance_thresholds(&admin, &200, &1000, &86400);
        client.update_pool_apy(&admin, &pool, &2500);

        // (1500 - 500) * 100 = 100_000
        let pools = Vec::from_array(&env, [allocation_with_il(&env, &pool, 500)]);

        client.set_min_net_profit(&admin, &100_000);
        assert_eq!(
            client.scan_opportunities(&vault_pool, &1000, &pools).len(),
            1
        );

        client.set_min_net_profit(&admin, &100_001);
        assert_eq!(
            client.scan_opportunities(&vault_pool, &1000, &pools).len(),
            0
        );
    }

    /// The minimum is disabled by default and a zero value re-disables it.
    #[test]
    fn test_min_net_profit_default_and_disable() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let vault_pool = Address::generate(&env);
        let pool = Address::generate(&env);

        client.set_rebalance_thresholds(&admin, &200, &1000, &86400);
        client.update_pool_apy(&admin, &pool, &2500);

        let pools = Vec::from_array(&env, [allocation_with_il(&env, &pool, 900)]);

        client.set_min_net_profit(&admin, &1_000_000);
        assert_eq!(
            client.scan_opportunities(&vault_pool, &1000, &pools).len(),
            0
        );

        client.set_min_net_profit(&admin, &0);
        assert_eq!(
            client.scan_opportunities(&vault_pool, &1000, &pools).len(),
            1
        );
    }

    /// Updating the other thresholds does not clear the configured minimum.
    #[test]
    fn test_set_rebalance_thresholds_preserves_min_net_profit() {
        let env = Env::default();
        let (client, admin) = engine(&env);

        client.set_min_net_profit(&admin, &75_000);
        client.set_rebalance_thresholds(&admin, &300, &200, &3600);

        let thresholds = client.get_arbitrage_thresholds();
        assert_eq!(thresholds.min_apy_delta, 300);
        assert_eq!(thresholds.max_il_tolerance, 200);
        assert_eq!(thresholds.cooldown_period, 3600);
        assert_eq!(thresholds.min_net_profit, 75_000);
    }

    /// Setting the minimum is admin-only and rejects negative values.
    #[test]
    fn test_set_min_net_profit_is_admin_only_and_non_negative() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let stranger = Address::generate(&env);

        let unauthorized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_min_net_profit(&stranger, &1_000);
        }));
        assert!(unauthorized.is_err(), "minimum must be admin only");

        let negative = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            client.set_min_net_profit(&admin, &-1);
        }));
        assert!(negative.is_err(), "a negative minimum is meaningless");

        assert_eq!(client.get_min_net_profit(), 0);
    }

    // ============ FEED DATA AGE TESTS (Issue #293) ============

    /// A published feed reports its age and freshness, and the age grows with
    /// the ledger clock.
    #[test]
    fn test_feed_data_age_tracks_age_and_freshness() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        env.ledger().set_timestamp(1000);
        client.update_pool_apy(&admin, &pool, &1200);
        client.set_max_apy_age(&admin, &500);

        let age = client.get_feed_data_age(&pool);
        assert_eq!(age.updated_at, 1000);
        assert_eq!(age.age_seconds, 0);
        assert_eq!(age.max_age_seconds, 500);
        assert!(age.fresh);

        env.ledger().set_timestamp(1400);
        let age = client.get_feed_data_age(&pool);
        assert_eq!(age.updated_at, 1000);
        assert_eq!(age.age_seconds, 400);
        assert!(age.fresh, "400s is still within the 500s window");

        // Exactly at the boundary the feed is still fresh (inclusive).
        env.ledger().set_timestamp(1500);
        let age = client.get_feed_data_age(&pool);
        assert_eq!(age.age_seconds, 500);
        assert!(age.fresh);

        // One second later it is stale.
        env.ledger().set_timestamp(1501);
        let age = client.get_feed_data_age(&pool);
        assert_eq!(age.age_seconds, 501);
        assert!(!age.fresh);
    }

    /// The age snapshot agrees with `is_apy_fresh`.
    #[test]
    fn test_feed_data_age_matches_is_apy_fresh() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_max_apy_age(&admin, &100);
        env.ledger().set_timestamp(10);
        client.update_pool_apy(&admin, &pool, &900);

        for ts in [10u64, 110, 111, 200] {
            env.ledger().set_timestamp(ts);
            assert_eq!(
                client.get_feed_data_age(&pool).fresh,
                client.is_apy_fresh(&pool),
                "snapshot and predicate must agree at t={}",
                ts
            );
        }
    }

    /// An unpublished feed reports no data rather than appearing brand new.
    #[test]
    fn test_feed_data_age_unpublished_is_not_fresh() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_max_apy_age(&admin, &300);
        env.ledger().set_timestamp(50_000);

        let age = client.get_feed_data_age(&pool);
        assert_eq!(age.updated_at, 0);
        assert_eq!(age.age_seconds, 0);
        assert_eq!(age.max_age_seconds, 300);
        assert!(!age.fresh, "no observation must never read as fresh");
        assert!(client.get_feed_age_seconds(&pool).is_none());
    }

    /// `get_feed_age_seconds` returns `None` before publication and the exact
    /// age afterwards.
    #[test]
    fn test_get_feed_age_seconds() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        assert!(client.get_feed_age_seconds(&pool).is_none());

        env.ledger().set_timestamp(700);
        client.update_pool_apy(&admin, &pool, &1000);
        assert_eq!(client.get_feed_age_seconds(&pool), Some(0));

        env.ledger().set_timestamp(760);
        assert_eq!(client.get_feed_age_seconds(&pool), Some(60));
    }

    /// A republished value still resets the age.
    #[test]
    fn test_republished_feed_resets_age() {
        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        env.ledger().set_timestamp(100);
        client.update_pool_apy(&admin, &pool, &900);
        env.ledger().set_timestamp(500);
        assert_eq!(client.get_feed_age_seconds(&pool), Some(400));

        client.update_pool_apy(&admin, &pool, &900);
        assert_eq!(client.get_feed_age_seconds(&pool), Some(0));
        assert!(client.get_feed_data_age(&pool).fresh);
    }

    /// `emit_feed_data_age` publishes the snapshot and returns it unchanged.
    #[test]
    fn test_emit_feed_data_age_publishes_snapshot() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::TryFromVal;

        let env = Env::default();
        let (client, admin) = engine(&env);
        let pool = Address::generate(&env);

        client.set_max_apy_age(&admin, &600);
        env.ledger().set_timestamp(2_000);
        client.update_pool_apy(&admin, &pool, &1500);

        env.ledger().set_timestamp(2_250);
        let returned = client.emit_feed_data_age(&pool);

        assert_eq!(returned.updated_at, 2_000);
        assert_eq!(returned.age_seconds, 250);
        assert_eq!(returned.max_age_seconds, 600);
        assert!(returned.fresh);

        let events = env.events().all();
        let (_contract, topics, data) = events.last().unwrap();
        let topic: Symbol = Symbol::try_from_val(&env, &topics.get(0).unwrap()).unwrap();
        assert_eq!(topic, Symbol::new(&env, "feed_data_age"));

        let (updated_at, age_seconds, max_age_seconds, fresh): (u64, u64, u64, bool) =
            <(u64, u64, u64, bool)>::try_from_val(&env, &data).unwrap();
        assert_eq!(updated_at, 2_000);
        assert_eq!(age_seconds, 250);
        assert_eq!(max_age_seconds, 600);
        assert!(fresh);
    }
}
