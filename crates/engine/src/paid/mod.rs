//! The paid-call gate: billing clients are constructible only here, and every
//! provider request must carry a [`Reservation`] ticket durably issued by the
//! budget ledger (decisions D6 and D13).
//!
//! # Why this module is shaped the way it is
//!
//! - **Compile-time D6 (sealed trait + unforgeable token).**
//!   [`PaidProvider`] is [sealed]: real billing clients (Exa, Parallel, the
//!   OpenRouter judge) live in this module and cannot be implemented anywhere
//!   else, and every dispatch goes through [`PaidGate::call`], which reserves
//!   budget first. The provider only ever sees an [`Attempt`] token that wraps
//!   the ledger-issued [`Reservation`]; the token has no public constructor and
//!   the ticket itself cannot be minted outside `store::ledger`. There is
//!   therefore no type-level way to reach a paid API without a reservation.
//! - **Fail closed.** The reservation row is committed to SQLite *before* the
//!   provider is dispatched. If the ledger cannot be written (or the budget is
//!   unknown, or the join fails), the gate returns
//!   [`PaidOutcome::LedgerUnavailable`] and the provider is never called.
//!   A cap refusal returns [`PaidOutcome::SkippedBudget`], also with zero
//!   provider requests.
//! - **Liabilities survive everything.** One ticket = one provider + its
//!   request bounds + one physical attempt; every retry reserves again (the
//!   gate performs no automatic retries — the controller decides, and each
//!   retry is a fresh [`PaidGate::call`]). An unsettled row keeps counting at
//!   its estimate through crashes, restarts, cancellation and ambiguous
//!   timeouts; dropping a [`Reservation`] releases nothing (there is no `Drop`
//!   impl on purpose). A late or unknown charge keeps the liability open until
//!   [`Ledger::settle_by_id`](crate::store::ledger::Ledger::settle_by_id)
//!   reconciles it, and a configured expiry charges the estimate.
//! - **Disclosure metadata.** Every attempt yields a [`PaidCallRecord`]
//!   (provider, reservation id, actual or estimated cost, settlement state):
//!   this is the cost evidence the evidence package will carry. *When* a paid
//!   call is triggered at all is the controller's job, not this gate's.
//!
//! Eval reference clients use the very same gate against a separate eval
//! budget ([`EVAL_BUDGET`] vs [`OPERATING_BUDGET`]), so eval runs can exhaust
//! their own cap without touching operating spend.
//!
//! The gate is async on tokio (the design's named runtime): the blocking
//! SQLite ledger runs on [`tokio::task::spawn_blocking`], so ledger work never
//! runs on the async runtime. The [`tokio::time::timeout`] deadline is
//! cooperative and bounds **only the provider call** — not the reservation or
//! the settlement — so provider implementations must be non-blocking async
//! code that yields; a provider that blocks its thread can still stall a
//! runtime worker.
//!
//! [sealed]: https://rust-lang.github.io/api-guidelines/future-proofing.html#sealed-traits-protect-against-downstream-implementations-c-sealed

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::store::ledger::{BudgetLedger, Ledger, LedgerError, Micros, Reservation};

/// Budget name for real (operating) paid calls.
///
/// A budget's monthly cap is configured with
/// [`Ledger::set_budget`](crate::store::ledger::Ledger::set_budget).
pub const OPERATING_BUDGET: &str = "operating";

/// Budget name for eval reference calls (the Parallel/Exa baselines).
///
/// Kept separate from [`OPERATING_BUDGET`] so evaluation spend can exhaust its
/// own cap without ever touching production search spend.
pub const EVAL_BUDGET: &str = "eval";

/// What the provider reports about the charge for one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeReport {
    /// The provider reported the exact charge for this attempt, in micro-USD.
    Actual(Micros),
    /// The provider did not report a charge. The attempt may still have been
    /// billed (an ambiguous timeout, a lost response), so the ledger liability
    /// must stay open at its estimate.
    Unknown,
}

/// A successful provider response plus what it says it charged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReply<R> {
    /// The provider's response body.
    pub body: R,
    /// What the provider says this attempt cost.
    pub charge: ChargeReport,
}

/// A provider-side failure plus what it says it charged.
///
/// A failure with [`ChargeReport::Unknown`] means "possibly charged": the
/// attempt may have been billed even though it failed, so the liability stays
/// open at its estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// Human-readable failure summary (for `PaidOutcome::Failed`).
    pub message: String,
    /// What the provider says this attempt cost.
    pub charge: ChargeReport,
}

/// The unforgeable dispatch token a provider receives for exactly one attempt.
///
/// It wraps the ledger [`Reservation`] the gate reserved before the dispatch:
/// [`Attempt::reservation_id`] doubles as the provider-side idempotency key,
/// and [`Attempt::max_cost_micros`] is the code-set request bound the attempt
/// was reserved against. Only [`PaidGate::call`] can create one (the field is
/// private and there is no public constructor), so a provider implementation
/// can never be handed an unreserved attempt.
///
/// ```compile_fail,E0451
/// use engine::paid::Attempt;
/// use engine::store::ledger::Reservation;
///
/// fn forge(ticket: &Reservation) -> Attempt<'_> {
///     // Fails with E0451: `Attempt`'s fields are private.
///     Attempt { reservation: ticket }
/// }
///
/// fn main() {}
/// ```
#[derive(Debug)]
pub struct Attempt<'a> {
    reservation: &'a Reservation,
}

impl<'a> Attempt<'a> {
    /// Wrap a ledger ticket; only the gate does this.
    fn new(reservation: &'a Reservation) -> Self {
        Self { reservation }
    }

    /// Id of the backing reservation row; providers use it as their
    /// idempotency key so a retried HTTP request cannot double-charge the
    /// attempt it stands for.
    pub fn reservation_id(&self) -> &str {
        self.reservation.id()
    }

    /// The request bound (maximum expected cost of this one attempt) that was
    /// reserved before dispatch, in micro-USD.
    pub fn max_cost_micros(&self) -> Micros {
        self.reservation.max_cost_micros()
    }
}

/// Sealed trait marker: [`PaidProvider`] can only be implemented inside the
/// `paid` module, so billing clients cannot be built or impersonated elsewhere.
mod sealed {
    /// Implemented only by the billing clients in this module.
    pub trait Sealed {}
}

/// A billing client (Exa, Parallel, the OpenRouter judge, ...) that only the
/// paid gate may dispatch.
///
/// The supertrait bound is `sealed::Sealed`, a private marker implemented only
/// by the real clients in this module: outside the crate the trait cannot be
/// implemented, which is the compile-time half of decision D6. Inside the
/// crate, every dispatch still has to go through [`PaidGate::call`] to obtain
/// an [`Attempt`], so no code path can reach a paid API without a ledger
/// reservation.
///
/// ```compile_fail,E0277
/// use std::future::Future;
/// use engine::paid::{Attempt, ChargeReport, PaidProvider, ProviderFailure, ProviderReply};
/// use engine::store::ledger::Micros;
///
/// struct Outside;
///
/// impl PaidProvider for Outside {
///     type Request = ();
///     type Response = ();
///     fn provider_id(&self) -> &'static str {
///         "outside"
///     }
///     fn max_cost_micros(&self, _req: &Self::Request) -> Micros {
///         1
///     }
///     fn send(
///         &self,
///         _attempt: Attempt<'_>,
///         _req: Self::Request,
///     ) -> impl Future<Output = Result<ProviderReply<Self::Response>, ProviderFailure>> + Send {
///         async { Ok(ProviderReply { body: (), charge: ChargeReport::Unknown }) }
///     }
/// }
///
/// fn main() {}
/// ```
pub trait PaidProvider: sealed::Sealed + Send + Sync {
    /// The provider's request type; consumed by exactly one attempt.
    type Request: Send;
    /// The provider's response body type.
    type Response: Send;

    /// Stable provider id recorded on the ledger row and the disclosure
    /// record (e.g. `"exa"`).
    fn provider_id(&self) -> &'static str;

    /// Code-set upper bound for one attempt, in micro-USD, for the given
    /// request. This is what the gate reserves, so it must cover the worst
    /// plausible charge of a single physical attempt; a non-positive bound is
    /// rejected by the ledger and fails the call closed.
    fn max_cost_micros(&self, req: &Self::Request) -> Micros;

    /// Perform exactly one physical attempt. The [`Attempt`] token is the
    /// idempotency handle (see [`Attempt::reservation_id`]); the caller's
    /// timeout wrapper decides what an unanswered call means, so this future
    /// may run as long as the provider takes.
    fn send(
        &self,
        attempt: Attempt<'_>,
        req: Self::Request,
    ) -> impl Future<Output = Result<ProviderReply<Self::Response>, ProviderFailure>> + Send;
}

/// Source of the Unix timestamps the ledger and the gate account with.
///
/// Injectable so tests can pin the clock; production uses [`SystemClock`].
pub trait Clock: Send + Sync {
    /// Current Unix time in seconds (UTC).
    fn now_unix(&self) -> i64;
}

/// The production [`Clock`]: current Unix time from the system clock (UTC).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> i64 {
        time::OffsetDateTime::now_utc().unix_timestamp()
    }
}

/// What the attempt actually cost, for the disclosure record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charge {
    /// The ledger has this exact charge on the row: what settlement recorded
    /// (the first settlement wins on an idempotent replay, so this can differ
    /// from the amount this call's provider reported).
    Actual(Micros),
    /// No exact charge is on the books yet: the cost is the reservation's
    /// `max_cost` estimate, which is what the ledger counts until a settlement
    /// records the real amount.
    Estimated(Micros),
}

/// Where the reservation row stands after the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// The ledger recorded the final charge; the liability is closed.
    Settled,
    /// The row was already settled before this call's charge arrived (an
    /// idempotent replay or a racing reconciliation): the earlier recorded
    /// amount stands and the liability is closed. The newly reported amount is
    /// disclosed here rather than discarded, but it is not recorded.
    AlreadySettled {
        /// The charge already on the row (what the books count).
        recorded_micros: Micros,
        /// What this call's provider reported, kept for audit/reconciliation.
        reported_micros: Micros,
    },
    /// The liability is still open — awaiting a late charge or a configured
    /// expiry — and counts at its estimate. This is the state after an
    /// unknown charge, an ambiguous timeout or a cancellation.
    Open,
    /// Settlement was attempted but the ledger write failed. The row stays
    /// open at its estimate and must be reconciled later (e.g. with
    /// [`Ledger::settle_by_id`](crate::store::ledger::Ledger::settle_by_id)).
    /// The reported charge is never discarded: it is kept here so a later
    /// reconciliation knows what to record.
    Deferred {
        /// The ledger error that made settlement impossible.
        error: String,
        /// What the provider reported this attempt charged, in micro-USD;
        /// reconcile the row at this amount later.
        reported_micros: Micros,
    },
}

/// Disclosure metadata for one paid attempt: provider, reservation id and
/// actual or estimated cost, plus where its ledger row stands.
///
/// This is the cost evidence the evidence package will carry per paid call;
/// *when* a paid call is triggered at all is the controller's job, not the
/// gate's. The record is produced for every outcome that consumed a
/// reservation ([`PaidOutcome::Ok`], [`PaidOutcome::Failed`],
/// [`PaidOutcome::Timeout`]); a refused reservation produces no record because
/// nothing was spent or dispatched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidCallRecord {
    /// Provider that was called (or, for a deferred settlement, was called and
    /// billed but could not be reconciled yet).
    pub provider: &'static str,
    /// Budget the reservation was drawn from ([`OPERATING_BUDGET`] or
    /// [`EVAL_BUDGET`]).
    pub budget: String,
    /// Id of the ledger reservation row backing this attempt.
    pub reservation_id: String,
    /// Code-set upper bound that was reserved before dispatch, in micro-USD.
    pub max_cost_micros: Micros,
    /// What this attempt cost: exact once settled, the estimate until then.
    pub charge: Charge,
    /// Where the ledger row stands after the call.
    pub settlement: Settlement,
    /// Where the provider's reported charge violated the reserved bound, in
    /// micro-USD: `Some(actual - max_cost)` whenever a reported actual exceeds
    /// the reserved `max_cost_micros` — a provider-bound defect, since the
    /// bound is code-set and was wrong — and `None` when the reported charge
    /// is within the bound or none was reported.
    ///
    /// The charge is deliberately **not clamped**: the ledger records
    /// above-bound actuals as-is when settlement succeeds, so the books show
    /// the real charge. If settlement fails, the row stays open at its
    /// `max_cost_micros` estimate and the books undercount by
    /// `over_bound_micros` until reconciliation. Callers and the controller
    /// must treat a `Some(_)` as a provider-bound defect. Decision D19 (user,
    /// 2026-10-12): a bound violation is disclosed and raised to the user for
    /// a decision; the engine never disables a provider automatically.
    pub over_bound_micros: Option<Micros>,
}

/// Result of one [`PaidGate::call`].
///
/// Every variant that dispatched carries a [`PaidCallRecord`] for disclosure;
/// [`PaidOutcome::SkippedBudget`] and [`PaidOutcome::LedgerUnavailable`] prove
/// that nothing was dispatched at all.
#[must_use]
#[derive(Debug)]
pub enum PaidOutcome<R> {
    /// The provider answered successfully. `body` is never lost, even when
    /// settlement had to be deferred; the record says whether the charge is
    /// actual or still the estimate.
    Ok {
        /// The provider's response body.
        body: R,
        /// Disclosure metadata for this attempt.
        record: PaidCallRecord,
    },
    /// The provider reported a failure. The message says what went wrong; the
    /// record says what it may have cost.
    Failed {
        /// Provider failure summary.
        message: String,
        /// Disclosure metadata for this attempt.
        record: PaidCallRecord,
    },
    /// The provider did not answer within `deadline`. The outcome is
    /// ambiguous: the liability stays open at its estimate because the request
    /// may have been dispatched and billed upstream.
    Timeout {
        /// Disclosure metadata for this attempt.
        record: PaidCallRecord,
    },
    /// The reservation was refused at the budget cap: the provider was NOT
    /// called and nothing was charged. This is the cost-control stop, not an
    /// error.
    SkippedBudget {
        /// Provider that would have been called.
        provider: &'static str,
        /// Budget that is at its cap.
        budget: String,
        /// Rejected request's maximum cost, in micro-USD.
        requested: Micros,
        /// Already-committed spend for the period, in micro-USD.
        committed: Micros,
        /// The budget's monthly cap, in micro-USD.
        cap: Micros,
    },
    /// The ledger could not record a reservation, so nothing was dispatched:
    /// fail closed, a paid call must never run ahead of its durable
    /// reservation.
    LedgerUnavailable {
        /// Provider that would have been called.
        provider: &'static str,
        /// The ledger error (storage failure, unknown budget, ...).
        error: String,
    },
}

/// The type-enforced paid-call gate (decisions D6 and D13).
///
/// One gate per budget: [`OPERATING_BUDGET`] for real paid calls and
/// [`EVAL_BUDGET`] for eval reference clients, backed by the same ledger but
/// with independent caps. Build one over the daemon's shared
/// [`Ledger`]; the generic parameter exists so tests can substitute a failing
/// [`BudgetLedger`] fake.
pub struct PaidGate<L: BudgetLedger = Ledger> {
    ledger: Arc<L>,
    budget: String,
    clock: Arc<dyn Clock>,
}

impl<L: BudgetLedger> PaidGate<L> {
    /// Build a gate that draws reservations from `budget`.
    ///
    /// The budget name must be non-empty; the cap itself is configured on the
    /// ledger with
    /// [`Ledger::set_budget`](crate::store::ledger::Ledger::set_budget).
    /// Fails with [`LedgerError::InvalidArgument`] for an empty name so a
    /// mistyped gate can never silently reserve against nothing.
    pub fn new(
        ledger: Arc<L>,
        budget: impl Into<String>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, LedgerError> {
        let budget = budget.into();
        if budget.is_empty() {
            return Err(LedgerError::InvalidArgument(
                "budget name must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            ledger,
            budget,
            clock,
        })
    }

    /// Perform one paid provider call under a hard cost gate.
    ///
    /// Sequence (D13):
    /// 1. Ask the provider for its code-set attempt bound and reserve that
    ///    maximum against the budget — on [`tokio::task::spawn_blocking`],
    ///    because the SQLite ledger is blocking and must not stall the async
    ///    runtime. The row is committed before the ticket exists; if the
    ///    reserving future is dropped mid-flight, any committed row simply
    ///    stays open and counted (fail-safe, never fail-lost).
    /// 2. [`LedgerError::OverCap`] becomes [`PaidOutcome::SkippedBudget`] and
    ///    any other ledger failure (unknown budget, storage error, join error)
    ///    becomes [`PaidOutcome::LedgerUnavailable`]. In both cases the
    ///    provider is NOT called.
    /// 3. Dispatch [`PaidProvider::send`] with an [`Attempt`] token under a
    ///    [`tokio::time::timeout`] of `deadline`.
    /// 4. Reconcile: an [`ChargeReport::Actual`] charge settles the ticket in
    ///    the ledger (idempotent, consumed on settle) and the record reports
    ///    the *recorded* charge: when a racing or replayed settlement got
    ///    there first, the first amount wins and is disclosed as
    ///    [`Settlement::AlreadySettled`] alongside the reported one.
    ///    [`ChargeReport::Unknown`] and timeouts deliberately leave the row
    ///    open at its estimate — possibly-charged and ambiguous calls must
    ///    keep their liability. If the settlement write itself fails, the body
    ///    is still returned with [`Settlement::Deferred`] (which keeps the
    ///    reported charge for later reconciliation) and the estimate stays
    ///    counted. A reported actual above the reserved bound is disclosed as
    ///    [`PaidCallRecord::over_bound_micros`], never clamped.
    ///
    /// Cancellation: dropping this future never releases a liability — the
    /// [`Reservation`] has no `Drop` impl that could free it, so spend can
    /// never disappear from the books. A committed row stays open (counted at
    /// its estimate) unless a settlement had already started on the blocking
    /// pool, which then completes and records the reported charge. Reconcile
    /// open rows later via
    /// [`Ledger::settle_by_id`](crate::store::ledger::Ledger::settle_by_id) or
    /// let a configured expiry charge the estimate.
    ///
    /// The gate performs **no automatic retries**: each retry is the caller's
    /// decision and a new `call`, i.e. a new reservation (one ticket = one
    /// physical attempt).
    pub async fn call<P: PaidProvider>(
        &self,
        provider: &P,
        req: P::Request,
        deadline: Duration,
    ) -> PaidOutcome<P::Response> {
        let provider_id = provider.provider_id();
        let requested = provider.max_cost_micros(&req);
        let now = self.clock.now_unix();

        // Durable reserve-before-dispatch. The blocking ledger write runs off
        // the async runtime.
        let ledger = Arc::clone(&self.ledger);
        let budget = self.budget.clone();
        let reserve = tokio::task::spawn_blocking(move || {
            ledger.reserve(&budget, provider_id, requested, now)
        })
        .await;
        let ticket = match reserve {
            Ok(Ok(ticket)) => ticket,
            Ok(Err(LedgerError::OverCap {
                budget,
                cap,
                committed,
                requested,
            })) => {
                return PaidOutcome::SkippedBudget {
                    provider: provider_id,
                    budget,
                    requested,
                    committed,
                    cap,
                };
            }
            Ok(Err(err)) => {
                return PaidOutcome::LedgerUnavailable {
                    provider: provider_id,
                    error: err.to_string(),
                };
            }
            Err(join_error) => {
                return PaidOutcome::LedgerUnavailable {
                    provider: provider_id,
                    error: join_error.to_string(),
                };
            }
        };

        // Defense in depth behind the sealed ledger: an in-crate bug must not
        // let this gate dispatch against a ticket minted for a different
        // provider or amount. On mismatch nothing is dispatched and the row is
        // NOT settled — it stays counted until it is reconciled.
        if ticket.provider() != provider_id || ticket.max_cost_micros() != requested {
            return PaidOutcome::LedgerUnavailable {
                provider: provider_id,
                error: format!(
                    "ledger reservation {} does not match the request (provider {:?} vs {provider_id:?}, max_cost {} vs {requested}); the row stays counted and must be reconciled",
                    ticket.id(),
                    ticket.provider(),
                    ticket.max_cost_micros(),
                ),
            };
        }

        let record = PaidCallRecord {
            provider: provider_id,
            budget: self.budget.clone(),
            reservation_id: ticket.id().to_owned(),
            max_cost_micros: ticket.max_cost_micros(),
            charge: Charge::Estimated(ticket.max_cost_micros()),
            settlement: Settlement::Open,
            over_bound_micros: None,
        };

        // One physical attempt. The `Attempt` borrows the ticket, and the
        // borrow ends here: the ticket is free to be moved into settlement.
        let attempt = Attempt::new(&ticket);
        let dispatched = tokio::time::timeout(deadline, provider.send(attempt, req)).await;

        match dispatched {
            Ok(Ok(ProviderReply { body, charge })) => {
                let over_bound_micros = over_bound(charge, record.max_cost_micros);
                let (charge, settlement) = self.reconcile(ticket, charge).await;
                PaidOutcome::Ok {
                    body,
                    record: PaidCallRecord {
                        charge,
                        settlement,
                        over_bound_micros,
                        ..record
                    },
                }
            }
            Ok(Err(ProviderFailure { message, charge })) => {
                let over_bound_micros = over_bound(charge, record.max_cost_micros);
                let (charge, settlement) = self.reconcile(ticket, charge).await;
                PaidOutcome::Failed {
                    message,
                    record: PaidCallRecord {
                        charge,
                        settlement,
                        over_bound_micros,
                        ..record
                    },
                }
            }
            Err(_elapsed) => {
                // Ambiguous: the request may have been dispatched and billed.
                // Dropping the ticket leaves the row open at its estimate.
                PaidOutcome::Timeout { record }
            }
        }
    }

    /// Settle the ticket if the provider reported an actual charge; otherwise
    /// leave the row open and counted at its estimate.
    ///
    /// The receipt's recorded charge is what the books count: on a racing or
    /// replayed settlement the first amount wins and the record reports it
    /// next to the newly reported one ([`Settlement::AlreadySettled`]). A
    /// failed settlement write (or a failed blocking join) keeps the row open
    /// too and reports [`Settlement::Deferred`], which carries the reported
    /// charge so it is never lost: the books count the estimate until a later
    /// reconciliation records the real charge, and the caller keeps its
    /// response body either way.
    async fn reconcile(&self, ticket: Reservation, reported: ChargeReport) -> (Charge, Settlement) {
        let estimate = ticket.max_cost_micros();
        match reported {
            ChargeReport::Unknown => {
                // Deliberately drop the ticket without settling: possibly
                // charged, so the liability must stay open at the estimate.
                (Charge::Estimated(estimate), Settlement::Open)
            }
            ChargeReport::Actual(reported_micros) => {
                let now = self.clock.now_unix();
                let ledger = Arc::clone(&self.ledger);
                let settled = tokio::task::spawn_blocking(move || {
                    ledger.settle(ticket, reported_micros, now)
                })
                .await;
                match settled {
                    Ok(Ok(receipt)) => {
                        let charge = Charge::Actual(receipt.recorded_micros);
                        // Every replay is disclosed, even when the amounts
                        // agree: this call's settle did not write the row.
                        let settlement = if receipt.already_settled {
                            Settlement::AlreadySettled {
                                recorded_micros: receipt.recorded_micros,
                                reported_micros,
                            }
                        } else {
                            Settlement::Settled
                        };
                        (charge, settlement)
                    }
                    Ok(Err(err)) => (
                        Charge::Estimated(estimate),
                        Settlement::Deferred {
                            error: err.to_string(),
                            reported_micros,
                        },
                    ),
                    Err(join_error) => (
                        Charge::Estimated(estimate),
                        Settlement::Deferred {
                            error: join_error.to_string(),
                            reported_micros,
                        },
                    ),
                }
            }
        }
    }
}

/// Bound-violation disclosure for one attempt: `Some(actual - max_cost)` when
/// the provider-reported actual exceeds the request bound that was reserved
/// (the provider's code-set bound is wrong), `None` when the reported charge
/// is within the bound or none was reported. The charge is never clamped: the
/// ledger records the real amount.
fn over_bound(reported: ChargeReport, max_cost_micros: Micros) -> Option<Micros> {
    match reported {
        ChargeReport::Actual(actual) if actual > max_cost_micros => Some(actual - max_cost_micros),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
