/**
 * Address-filtered contract event listener.
 *
 * Operators running multiple contracts often share a single polling loop
 * against the Soroban RPC. Without filtering, every poll returns every
 * contract event on the network, flooding logs with noise from unrelated
 * contracts.  This module lets a listener declare exactly which contract
 * addresses it cares about; events from every other address are silently
 * discarded before reaching registered handlers.
 *
 * ## Usage
 *
 * ```ts
 * const listener = new ContractEventListener(rpcServer, {
 *   contractAddresses: [VAULT_A, VAULT_B],
 *   startLedger: currentLedger,
 * });
 *
 * listener.on('deposit', (evt) => console.log('deposit on', evt.contractId));
 * listener.on('harvest', (evt) => console.log('harvest on', evt.contractId));
 *
 * listener.start(5_000); // poll every 5 s
 * // ...later:
 * listener.stop();
 * ```
 */

import type { VaultEvent, RebalanceEvent, RewardEvent } from './types';

// ---------------------------------------------------------------------------
// Soroban RPC surface
// ---------------------------------------------------------------------------

/**
 * A single raw contract event as returned by `getEvents`. Only the fields
 * this module reads are declared; `SorobanRpc.Api.EventRecord` satisfies
 * this interface, and so does a stub in tests.
 */
export interface RawContractEvent {
  /** Bech32-encoded or C-strA address of the emitting contract. */
  contractId: string;
  /** Ledger sequence in which the event was emitted. */
  ledger: number;
  /** Ledger-close timestamp (Unix seconds) of the emitting ledger. */
  ledgerClosedAt: string;
  /**
   * The opaque event topic/value fields. Handlers that understand the specific
   * contract ABI can interpret them; the listener itself treats them as
   * pass-through data.
   */
  topic?: unknown[];
  value?: unknown;
  /** Raw ID string from the RPC, used for deduplication. */
  id?: string;
}

/**
 * Minimal structural interface for the Soroban `getEvents` call.
 * `SorobanRpc.Server` satisfies it, and so does any hand-rolled stub.
 */
export interface EventSource {
  getEvents(request: GetEventsRequest): Promise<GetEventsResponse>;
}

export interface GetEventsFilter {
  type?: 'contract' | 'system' | 'diagnostic' | 'all';
  contractIds?: string[];
  topics?: string[][];
}

export interface GetEventsRequest {
  startLedger: number;
  endLedger?: number;
  filters?: GetEventsFilter[];
  limit?: number;
}

export interface GetEventsResponse {
  events: RawContractEvent[];
  latestLedger: number;
}

// ---------------------------------------------------------------------------
// Typed SDK events — enrich raw RPC events with the contract address
// ---------------------------------------------------------------------------

/** A VaultEvent enriched with the contract address that emitted it. */
export type FilteredVaultEvent = VaultEvent & { contractId: string };

/** A RebalanceEvent enriched with the contract address that emitted it. */
export type FilteredRebalanceEvent = RebalanceEvent & { contractId: string };

/** A RewardEvent enriched with the contract address that emitted it. */
export type FilteredRewardEvent = RewardEvent & { contractId: string };

/**
 * Catch-all for events whose type is not one of the well-known SDK event
 * shapes. Handlers receive the raw RPC payload.
 */
export interface UnknownContractEvent {
  contractId: string;
  ledger: number;
  ledgerClosedAt: string;
  topic?: unknown[];
  value?: unknown;
}

// ---------------------------------------------------------------------------
// Handler registry
// ---------------------------------------------------------------------------

/** All typed event names recognised by the listener. */
export type KnownEventName =
  | VaultEvent['type']        // 'deposit' | 'withdraw' | 'harvest' | 'pause' | 'unpause'
  | RebalanceEvent['type']    // 'proposal' | 'execution' | 'failure'
  | RewardEvent['type'];      // 'distribution' | 'claim' | 'expiry'

/** Map from event name to its handler payload type. */
export interface EventHandlerMap {
  deposit: FilteredVaultEvent;
  withdraw: FilteredVaultEvent;
  harvest: FilteredVaultEvent;
  pause: FilteredVaultEvent;
  unpause: FilteredVaultEvent;
  proposal: FilteredRebalanceEvent;
  execution: FilteredRebalanceEvent;
  failure: FilteredRebalanceEvent;
  distribution: FilteredRewardEvent;
  claim: FilteredRewardEvent;
  expiry: FilteredRewardEvent;
}

export type EventHandler<K extends KnownEventName> = (event: EventHandlerMap[K]) => void;

/** Handler called for every raw event that passes the address filter. */
export type RawEventHandler = (event: UnknownContractEvent) => void;

// ---------------------------------------------------------------------------
// Listener configuration
// ---------------------------------------------------------------------------

/**
 * Configuration for `ContractEventListener`.
 *
 * The only required field is `startLedger`; everything else has a sensible
 * default.
 */
export interface ListenerConfig {
  /**
   * Restrict event delivery to this set of contract addresses. When provided
   * and non-empty, any event from a contract **not** in this list is silently
   * dropped before reaching handlers. When omitted or empty, events from all
   * contracts are forwarded.
   *
   * Addresses should be in the same format returned by the RPC (bech32 / StrKey
   * C-address). Comparison is case-insensitive.
   */
  contractAddresses?: string[];

  /**
   * Ledger sequence to start reading from. The listener advances its internal
   * cursor forward as ledgers are consumed so restarts do not re-process old
   * events.
   */
  startLedger: number;

  /**
   * Maximum number of events to request per poll. Soroban's default ceiling is
   * 10 000; the listener defaults to 200 to keep individual payloads small.
   */
  pageSize?: number;

  /**
   * Additional RPC-level event filter (e.g. event type or topic). The
   * `contractIds` inside this filter is merged with (or overridden by)
   * `contractAddresses` — do **not** set both; use `contractAddresses`.
   */
  rpcFilter?: Omit<GetEventsFilter, 'contractIds'>;
}

// ---------------------------------------------------------------------------
// Error event
// ---------------------------------------------------------------------------

export interface ListenerErrorEvent {
  error: Error;
  /** Current cursor (ledger) at the time of the error. */
  ledger: number;
}

// ---------------------------------------------------------------------------
// ContractEventListener
// ---------------------------------------------------------------------------

/**
 * Polls the Soroban RPC for contract events and dispatches them to registered
 * handlers, optionally restricting delivery to a declared set of contract
 * addresses.
 *
 * ### Filtering
 *
 * Address filtering happens at two levels:
 *
 * 1. **RPC level** (preferred): When `contractAddresses` is provided, the
 *    addresses are forwarded to the RPC `getEvents` filter so the node itself
 *    narrows the result set before any bytes travel over the wire.
 * 2. **SDK level** (belt-and-suspenders): Before dispatching, the listener
 *    checks `event.contractId` against the canonical (lower-cased) allow-list
 *    and drops any event that slips through.
 *
 * ### Event dispatch
 *
 * The listener inspects each raw event's first topic element for a string that
 * matches a known SDK event name (`deposit`, `harvest`, `proposal`, …) and
 * calls the corresponding typed handler. If the topic does not map to a known
 * name the `raw` handler is called instead, giving callers the full payload.
 *
 * ### Error handling
 *
 * Poll errors are delivered through the `error` handler if one is registered.
 * If no error handler is registered, errors are logged to `console.error` and
 * the poll loop continues. The listener never stops itself on error.
 */
export class ContractEventListener {
  private readonly source: EventSource;
  private readonly config: ListenerConfig;

  /** Lower-cased canonical address allow-list (empty ⟹ pass-all). */
  private readonly allowedAddresses: Set<string>;

  /** Latest ledger the listener has consumed (exclusive lower bound). */
  private cursor: number;

  /** Typed-event handler registry. */
  private readonly handlers = new Map<KnownEventName, Set<EventHandler<KnownEventName>>>();

  /** Catch-all handler for events whose name is unknown. */
  private rawHandlers: Set<RawEventHandler> = new Set();

  /** Error handlers. */
  private errorHandlers: Set<(e: ListenerErrorEvent) => void> = new Set();

  /** Timer handle returned by `setInterval`, null when stopped. */
  private intervalHandle: ReturnType<typeof setInterval> | null = null;

  constructor(source: EventSource, config: ListenerConfig) {
    this.source = source;
    this.config = config;
    this.cursor = config.startLedger;

    // Normalise addresses once for O(1) look-ups at event time.
    this.allowedAddresses = new Set(
      (config.contractAddresses ?? []).map((a) => a.toLowerCase()),
    );
  }

  // -------------------------------------------------------------------------
  // Handler registration
  // -------------------------------------------------------------------------

  /**
   * Register a handler for a specific typed event name.
   *
   * Multiple handlers for the same name are all called in registration order.
   * Returns `this` for chaining.
   */
  on<K extends KnownEventName>(name: K, handler: EventHandler<K>): this {
    if (!this.handlers.has(name)) {
      this.handlers.set(name, new Set());
    }
    (this.handlers.get(name) as Set<EventHandler<K>>).add(handler);
    return this;
  }

  /**
   * Remove a previously registered handler.  If `handler` was not registered,
   * this is a no-op. Returns `this` for chaining.
   */
  off<K extends KnownEventName>(name: K, handler: EventHandler<K>): this {
    (this.handlers.get(name) as Set<EventHandler<K>> | undefined)?.delete(handler);
    return this;
  }

  /**
   * Register a catch-all handler that receives every event that passes the
   * address filter, regardless of whether its name is a known SDK event.
   */
  onRaw(handler: RawEventHandler): this {
    this.rawHandlers.add(handler);
    return this;
  }

  /** Remove a raw handler. */
  offRaw(handler: RawEventHandler): this {
    this.rawHandlers.delete(handler);
    return this;
  }

  /**
   * Register an error handler. Multiple handlers are all called.
   * If no handler is registered, errors fall through to `console.error`.
   */
  onError(handler: (e: ListenerErrorEvent) => void): this {
    this.errorHandlers.add(handler);
    return this;
  }

  // -------------------------------------------------------------------------
  // Lifecycle
  // -------------------------------------------------------------------------

  /**
   * Start polling the Soroban RPC at the given interval.
   *
   * @param intervalMs Milliseconds between polls (default: 5000 ms / 1 ledger).
   */
  start(intervalMs = 5_000): void {
    if (this.intervalHandle !== null) return; // already running
    // Poll immediately on start, then on the interval.
    void this.poll();
    this.intervalHandle = setInterval(() => void this.poll(), intervalMs);
  }

  /** Stop polling. In-flight polls complete normally. */
  stop(): void {
    if (this.intervalHandle !== null) {
      clearInterval(this.intervalHandle);
      this.intervalHandle = null;
    }
  }

  /** Whether the listener is currently polling. */
  get isRunning(): boolean {
    return this.intervalHandle !== null;
  }

  /** The ledger the listener will start from on the next poll. */
  get currentLedger(): number {
    return this.cursor;
  }

  // -------------------------------------------------------------------------
  // Core polling logic
  // -------------------------------------------------------------------------

  /**
   * Perform a single poll against the RPC and dispatch resulting events.
   *
   * This is public so it can be called manually in tests or scripts that
   * manage their own timing.
   */
  async poll(): Promise<void> {
    try {
      const request = this.buildRequest();
      const response = await this.source.getEvents(request);
      this.processResponse(response);
    } catch (err) {
      const error = err instanceof Error ? err : new Error(String(err));
      const event: ListenerErrorEvent = { error, ledger: this.cursor };
      if (this.errorHandlers.size > 0) {
        this.errorHandlers.forEach((h) => h(event));
      } else {
        // eslint-disable-next-line no-console
        console.error(`[ContractEventListener] poll error at ledger ${this.cursor}:`, error.message);
      }
    }
  }

  // -------------------------------------------------------------------------
  // Private helpers
  // -------------------------------------------------------------------------

  private buildRequest(): GetEventsRequest {
    const filter: GetEventsFilter = {
      ...(this.config.rpcFilter ?? {}),
      type: (this.config.rpcFilter as GetEventsFilter | undefined)?.type ?? 'contract',
    };

    // Push address filtering down to the RPC layer when we have an allow-list.
    if (this.allowedAddresses.size > 0) {
      filter.contractIds = [...this.allowedAddresses];
    }

    return {
      startLedger: this.cursor,
      filters: [filter],
      limit: this.config.pageSize ?? 200,
    };
  }

  private processResponse(response: GetEventsResponse): void {
    const { events, latestLedger } = response;

    for (const raw of events) {
      // SDK-level guard: drop events not in the allow-list.
      if (!this.isAllowed(raw.contractId)) continue;

      const unknown: UnknownContractEvent = {
        contractId: raw.contractId,
        ledger: raw.ledger,
        ledgerClosedAt: raw.ledgerClosedAt,
        topic: raw.topic,
        value: raw.value,
      };

      // Deliver to raw handlers unconditionally (all passing events).
      this.rawHandlers.forEach((h) => h(unknown));

      // Attempt typed dispatch.
      const name = this.extractEventName(raw);
      if (name !== null) {
        this.dispatchTyped(name, raw);
      }
    }

    // Advance the cursor past the latest ledger so the next poll does not
    // re-fetch the same events.  We only advance when the RPC reports a
    // later ledger than our current cursor.
    if (latestLedger > this.cursor) {
      this.cursor = latestLedger + 1;
    }
  }

  /**
   * Returns true when:
   * - no allow-list is configured (pass-all mode), or
   * - the event's contract address is in the allow-list.
   */
  private isAllowed(contractId: string): boolean {
    if (this.allowedAddresses.size === 0) return true;
    return this.allowedAddresses.has(contractId.toLowerCase());
  }

  /**
   * Extract the SDK event name from the first element of the event's topic
   * array. Returns `null` when no match can be made.
   *
   * Soroban contract events encode their "event name" as the first topic
   * element (a Symbol or String XDR value). The stellar-sdk decodes it to a
   * plain JS string, which is what we compare here.
   */
  private extractEventName(raw: RawContractEvent): KnownEventName | null {
    const topic = raw.topic;
    if (!Array.isArray(topic) || topic.length === 0) return null;

    const first = topic[0];
    if (typeof first !== 'string') return null;

    const known: KnownEventName[] = [
      'deposit', 'withdraw', 'harvest', 'pause', 'unpause',
      'proposal', 'execution', 'failure',
      'distribution', 'claim', 'expiry',
    ];

    return (known as string[]).includes(first) ? (first as KnownEventName) : null;
  }

  /**
   * Build a typed SDK event from the raw RPC event and call all registered
   * handlers for the given name.
   */
  private dispatchTyped(name: KnownEventName, raw: RawContractEvent): void {
    const set = this.handlers.get(name);
    if (!set || set.size === 0) return;

    const base = {
      contractId: raw.contractId,
      timestamp: Math.floor(new Date(raw.ledgerClosedAt).getTime() / 1000),
      data: raw.value,
    };

    // Build the narrowest typed event we can without a full ABI decoder.
    // For the purposes of filtering and routing, the contractId + timestamp +
    // raw data are sufficient; callers that need decoded fields should attach
    // a raw handler and decode themselves.
    const vaultNames: KnownEventName[] = ['deposit', 'withdraw', 'harvest', 'pause', 'unpause'];
    const rebalanceNames: KnownEventName[] = ['proposal', 'execution', 'failure'];
    const rewardNames: KnownEventName[] = ['distribution', 'claim', 'expiry'];

    let evt: EventHandlerMap[KnownEventName];

    if ((vaultNames as string[]).includes(name)) {
      evt = {
        type: name as VaultEvent['type'],
        vault: raw.contractId as unknown as import('stellar-sdk').Address,
        timestamp: base.timestamp,
        data: base.data,
        contractId: raw.contractId,
      } satisfies FilteredVaultEvent;
    } else if ((rebalanceNames as string[]).includes(name)) {
      evt = {
        type: name as RebalanceEvent['type'],
        strategy: 0, // decoded by a full ABI consumer; we provide the raw data
        timestamp: base.timestamp,
        data: base.data,
        contractId: raw.contractId,
      } satisfies FilteredRebalanceEvent;
    } else {
      // rewardNames
      evt = {
        type: name as RewardEvent['type'],
        vault: raw.contractId as unknown as import('stellar-sdk').Address,
        timestamp: base.timestamp,
        data: base.data,
        contractId: raw.contractId,
      } satisfies FilteredRewardEvent;
    }

    set.forEach((h) => (h as (e: typeof evt) => void)(evt));
  }
}
