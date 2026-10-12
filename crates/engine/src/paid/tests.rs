//! Offline contract tests for the paid-call gate (decisions D6 and D13).
//!
//! Every provider is a mock and every ledger is a `tempfile` database on disk:
//! no network, no live or paper API, no real billing client. The fakes are
//! in-crate (the only place a `BudgetLedger` can be implemented) and can only
//! fail, park or delegate to a real [`Ledger`] — possibly with tampered
//! request parameters, which the gate's ticket check must catch:
//! [`Reservation`] cannot be minted outside `store::ledger`, so no test can
//! fabricate a ticket.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use time::{Date, Month, PrimitiveDateTime, Time};
use tokio::sync::Notify;

use super::sealed::Sealed;
use super::{
    Attempt, Charge, ChargeReport, Clock, EVAL_BUDGET, OPERATING_BUDGET, PaidGate, PaidOutcome,
    PaidProvider, ProviderFailure, ProviderReply, Settlement,
};
use crate::store::ledger::sealed::Sealed as LedgerSealed;
use crate::store::ledger::{
    BudgetLedger, Ledger, LedgerError, Micros, Reservation, ReservationState, SettleReceipt,
};

/// 2026-11-05T12:00:00Z, fixed so every period assertion is deterministic and
/// a local-time period derivation cannot pass by accident.
fn now() -> i64 {
    PrimitiveDateTime::new(
        Date::from_calendar_date(2026, Month::November, 5).unwrap(),
        Time::from_hms(12, 0, 0).unwrap(),
    )
    .assume_utc()
    .unix_timestamp()
}

/// The UTC accounting period [`now`] falls in.
fn period() -> &'static str {
    "2026-11"
}

/// A [`Clock`] pinned to [`now`] so ledger rows land in a known period.
struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0
    }
}

/// Fresh on-disk ledger with one budget configured.
fn setup(budget: &str, cap: Micros) -> (tempfile::TempDir, Arc<Ledger>) {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    ledger.set_budget(budget, cap).unwrap();
    (dir, ledger)
}

/// A gate over the real ledger, pinned to [`now`].
fn make_gate(ledger: Arc<Ledger>, budget: &str) -> PaidGate {
    PaidGate::new(ledger, budget, Arc::new(FixedClock(now()))).unwrap()
}

/// What the mock provider does when it is dispatched.
#[derive(Clone)]
enum Behavior {
    /// Answer and report an exact charge.
    ReplyActual {
        /// Response body.
        body: &'static str,
        /// Reported charge in micro-USD.
        charge: Micros,
    },
    /// Answer without reporting a charge (delayed charge upstream).
    ReplyUnknown,
    /// Fail and report an exact charge.
    FailActual {
        /// Failure message.
        message: &'static str,
        /// Reported charge in micro-USD.
        charge: Micros,
    },
    /// Fail without reporting a charge (possibly billed upstream).
    FailUnknown,
    /// Never answer; for timeout tests.
    Hang,
    /// Wait until the `Notify` is triggered; for cancellation tests.
    Wait(Arc<Notify>),
    /// Wait until the `Notify` is triggered, then answer and report an exact
    /// charge; for racing-reconciliation tests.
    WaitThenActual {
        /// Release signal.
        release: Arc<Notify>,
        /// Reported charge in micro-USD.
        charge: Micros,
    },
    /// Panic inside the provider; for the provider-panic join-error test.
    Panic,
}

/// What the durable-before-dispatch probe observed inside a provider dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowProbe {
    /// The probe opened an independent ledger connection and looked for the
    /// dispatched reservation id: `state` is the row's state, or `None` when
    /// no row with that id exists.
    Seen {
        /// The reservation id the dispatch carried.
        id: String,
        /// State of the row with that id, if it exists.
        state: Option<ReservationState>,
    },
    /// The probe could not open or read the ledger at all.
    Failed(String),
}

/// Mock billing client: counts every dispatch, records the reservation ids it
/// saw and behaves exactly as its [`Behavior`] says. With a durable probe it
/// also verifies — before doing anything else — that the reservation row was
/// durably committed, from an independent connection.
struct MockProvider {
    id: &'static str,
    max_cost: Micros,
    requests: AtomicUsize,
    seen: Mutex<Vec<String>>,
    behavior: Mutex<Behavior>,
    dispatched: Notify,
    durable_probe: Option<(PathBuf, String)>,
    probe_result: Mutex<Option<RowProbe>>,
}

impl MockProvider {
    fn new(id: &'static str, max_cost: Micros) -> Self {
        Self {
            id,
            max_cost,
            requests: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            behavior: Mutex::new(Behavior::ReplyActual {
                body: "evidence",
                charge: 100,
            }),
            dispatched: Notify::new(),
            durable_probe: None,
            probe_result: Mutex::new(None),
        }
    }

    fn with_behavior(self, behavior: Behavior) -> Self {
        *self.behavior.lock().unwrap() = behavior;
        self
    }

    /// Check, from an independent `Ledger::open(path)` connection inside
    /// `send`, that the dispatched reservation row exists and is `Open`
    /// before the request proceeds. The check result is recorded (never
    /// panics inside the provider) and asserted after the call.
    fn with_durable_probe(self, path: impl Into<PathBuf>, budget: &str) -> Self {
        Self {
            durable_probe: Some((path.into(), budget.to_owned())),
            ..self
        }
    }

    fn set_behavior(&self, behavior: Behavior) {
        *self.behavior.lock().unwrap() = behavior;
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn reservation_ids(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    /// The recorded durable-before-dispatch probe result, if the probe ran.
    fn durable_probe_result(&self) -> Option<RowProbe> {
        self.probe_result.lock().unwrap().clone()
    }

    /// Fires once per dispatch (with a stored permit, so tests cannot miss it).
    fn dispatched(&self) -> &Notify {
        &self.dispatched
    }
}

// Implementing the sealed trait is allowed here: this test module lives inside
// the `paid` module. Outside the crate the doctests prove it does not compile.
impl Sealed for MockProvider {}

impl PaidProvider for MockProvider {
    type Request = String;
    type Response = String;

    fn provider_id(&self) -> &'static str {
        self.id
    }

    fn max_cost_micros(&self, _req: &Self::Request) -> Micros {
        self.max_cost
    }

    async fn send(
        &self,
        attempt: Attempt<'_>,
        _req: Self::Request,
    ) -> Result<ProviderReply<Self::Response>, ProviderFailure> {
        // Durable-before-dispatch: the reservation row must already be
        // committed and open in an INDEPENDENT connection before the provider
        // does anything else. Recorded, never panicked, so the test can assert.
        if let Some((path, budget)) = &self.durable_probe {
            let probe = match Ledger::open(path) {
                Ok(ledger) => match ledger.reservations(budget) {
                    Ok(rows) => RowProbe::Seen {
                        id: attempt.reservation_id().to_owned(),
                        state: rows
                            .iter()
                            .find(|row| row.id == attempt.reservation_id())
                            .map(|row| row.state),
                    },
                    Err(err) => RowProbe::Failed(err.to_string()),
                },
                Err(err) => RowProbe::Failed(err.to_string()),
            };
            *self.probe_result.lock().unwrap() = Some(probe);
        }
        {
            self.requests.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap()
                .push(attempt.reservation_id().to_owned());
            let behavior = self.behavior.lock().unwrap().clone();
            self.dispatched.notify_one();
            match behavior {
                Behavior::ReplyActual { body, charge } => Ok(ProviderReply {
                    body: body.to_owned(),
                    charge: ChargeReport::Actual(charge),
                }),
                Behavior::ReplyUnknown => Ok(ProviderReply {
                    body: String::from("evidence-unknown-charge"),
                    charge: ChargeReport::Unknown,
                }),
                Behavior::FailActual { message, charge } => Err(ProviderFailure {
                    message: message.to_owned(),
                    charge: ChargeReport::Actual(charge),
                }),
                Behavior::FailUnknown => Err(ProviderFailure {
                    message: String::from("upstream failed"),
                    charge: ChargeReport::Unknown,
                }),
                Behavior::Hang => {
                    std::future::pending::<Result<ProviderReply<String>, ProviderFailure>>().await
                }
                Behavior::Wait(release) => {
                    release.notified().await;
                    Ok(ProviderReply {
                        body: String::from("released"),
                        charge: ChargeReport::Unknown,
                    })
                }
                Behavior::WaitThenActual { release, charge } => {
                    release.notified().await;
                    Ok(ProviderReply {
                        body: String::from("released"),
                        charge: ChargeReport::Actual(charge),
                    })
                }
                Behavior::Panic => panic!("provider exploded"),
            }
        }
    }
}

/// Simulated storage outage on every ledger write. This is the
/// ledger-write-failure seam the file-permission tests in `tests/ledger.rs`
/// cannot provoke after a successful `open`; the fake can only fail — it can
/// never mint a [`Reservation`] of its own.
struct BrokenReserveLedger;

// Implementing the sealed ledger trait is allowed here: this test module is
// part of the `engine` crate, the only place `BudgetLedger` can be
// implemented (the doctests prove outside crates cannot).
impl LedgerSealed for BrokenReserveLedger {}

impl BudgetLedger for BrokenReserveLedger {
    fn reserve(
        &self,
        _budget: &str,
        _provider: &str,
        _max_cost: Micros,
        _now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        Err(LedgerError::Storage(rusqlite::Error::InvalidQuery))
    }

    fn settle(
        &self,
        _reservation: Reservation,
        _actual: Micros,
        _now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        Err(LedgerError::Storage(rusqlite::Error::InvalidQuery))
    }
}

/// Ledger fake that reserves like a real ledger but whose settlement write
/// always fails: the body must come back and the liability must stay open.
struct BrokenSettleLedger {
    inner: Arc<Ledger>,
}

impl LedgerSealed for BrokenSettleLedger {}

impl BudgetLedger for BrokenSettleLedger {
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        self.inner.reserve(budget, provider, max_cost, now_unix)
    }

    fn settle(
        &self,
        _reservation: Reservation,
        _actual: Micros,
        _now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        Err(LedgerError::Storage(rusqlite::Error::InvalidQuery))
    }
}

/// Rendezvous for fake ledgers that must park inside a gate call: `entered`
/// fires when the fake is parked, `release` unparks it, and `finished` fires
/// just before the fake returns. Channel-based so a release can never race
/// ahead of the park.
struct Parking {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    finished: mpsc::Sender<()>,
}

/// Test-side handles for a [`Parking`] rendezvous.
struct ParkingControls {
    entered: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    finished: mpsc::Receiver<()>,
}

impl Parking {
    /// Park inside the fake's blocking seam until the test releases it.
    fn park(&self) {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        self.finished.send(()).unwrap();
    }
}

fn parking() -> (Arc<Parking>, ParkingControls) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    (
        Arc::new(Parking {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            finished: finished_tx,
        }),
        ParkingControls {
            entered: entered_rx,
            release: release_tx,
            finished: finished_rx,
        },
    )
}

/// In-crate [`BudgetLedger`] fake: reserves like a real ledger, then parks
/// before returning the ticket, so a test can abort the gate task
/// mid-reservation.
struct BlockingReserveLedger {
    inner: Arc<Ledger>,
    park: Arc<Parking>,
}

impl LedgerSealed for BlockingReserveLedger {}

impl BudgetLedger for BlockingReserveLedger {
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        // The row is committed BEFORE the park: a caller cancelled mid-reserve
        // still leaves a counted liability behind.
        let ticket = self.inner.reserve(budget, provider, max_cost, now_unix)?;
        self.park.park();
        Ok(ticket)
    }

    fn settle(
        &self,
        reservation: Reservation,
        actual: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        self.inner.settle(reservation, actual, now_unix)
    }
}

/// In-crate [`BudgetLedger`] fake: settles like a real ledger but parks
/// *before* delegating the write, so a test can abort the gate task while the
/// settlement is still in flight.
struct BlockingSettleLedger {
    inner: Arc<Ledger>,
    park: Arc<Parking>,
}

impl LedgerSealed for BlockingSettleLedger {}

impl BudgetLedger for BlockingSettleLedger {
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        self.inner.reserve(budget, provider, max_cost, now_unix)
    }

    fn settle(
        &self,
        reservation: Reservation,
        actual: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        self.park.park();
        self.inner.settle(reservation, actual, now_unix)
    }
}

/// In-crate [`BudgetLedger`] fake whose reserve panics inside the blocking
/// join: the gate must turn the join error into `LedgerUnavailable`.
struct PanicReserveLedger;

impl LedgerSealed for PanicReserveLedger {}

impl BudgetLedger for PanicReserveLedger {
    fn reserve(
        &self,
        _budget: &str,
        _provider: &str,
        _max_cost: Micros,
        _now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        panic!("reserve exploded")
    }

    fn settle(
        &self,
        _reservation: Reservation,
        _actual: Micros,
        _now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        Err(LedgerError::Storage(rusqlite::Error::InvalidQuery))
    }
}

/// In-crate [`BudgetLedger`] fake that reserves like a real ledger but whose
/// settle panics inside the blocking join: the body must still come back with
/// a deferred settlement that keeps the reported charge.
struct PanicSettleLedger {
    inner: Arc<Ledger>,
}

impl LedgerSealed for PanicSettleLedger {}

impl BudgetLedger for PanicSettleLedger {
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        self.inner.reserve(budget, provider, max_cost, now_unix)
    }

    fn settle(
        &self,
        _reservation: Reservation,
        _actual: Micros,
        _now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        panic!("settle exploded")
    }
}

/// How a [`MismatchedTicketLedger`] tampers with the reserved ticket.
#[derive(Debug, Clone, Copy)]
enum Mismatch {
    /// Reserve the right amount against a different provider.
    Provider,
    /// Reserve a different amount for the right provider.
    Amount,
}

/// In-crate [`BudgetLedger`] fake that delegates to a real [`Ledger`] but
/// returns a ticket minted for a *different* provider or amount than the
/// request. The gate must refuse to dispatch it; the mismatched row stays
/// counted and is never settled by the gate.
struct MismatchedTicketLedger {
    inner: Arc<Ledger>,
    mismatch: Mismatch,
}

impl LedgerSealed for MismatchedTicketLedger {}

impl BudgetLedger for MismatchedTicketLedger {
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        match self.mismatch {
            Mismatch::Provider => self
                .inner
                .reserve(budget, "someone-else", max_cost, now_unix),
            Mismatch::Amount => self.inner.reserve(budget, provider, max_cost + 1, now_unix),
        }
    }

    fn settle(
        &self,
        reservation: Reservation,
        actual: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        self.inner.settle(reservation, actual, now_unix)
    }
}

#[tokio::test]
async fn one_ledger_reservation_row_per_provider_request() {
    let (dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 400)
        .with_durable_probe(dir.path().join("ledger.db"), OPERATING_BUDGET)
        .with_behavior(Behavior::ReplyActual {
            body: "evidence",
            charge: 250,
        });

    let outcome = gate
        .call(&provider, String::from("query"), Duration::from_secs(5))
        .await;

    let record = match outcome {
        PaidOutcome::Ok { body, record } => {
            assert_eq!(body, "evidence");
            record
        }
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_eq!(record.provider, "exa");
    assert_eq!(record.budget, OPERATING_BUDGET);
    assert_eq!(record.max_cost_micros, 400);
    assert_eq!(record.charge, Charge::Actual(250));
    assert_eq!(record.settlement, Settlement::Settled);
    assert_eq!(record.over_bound_micros, None);
    assert!(!record.reservation_id.is_empty());

    // Durable before dispatch: the provider's independent connection saw the
    // reservation row committed and open before the request went out.
    assert_eq!(
        provider.durable_probe_result(),
        Some(RowProbe::Seen {
            id: record.reservation_id.clone(),
            state: Some(ReservationState::Open),
        })
    );

    // One provider request, one ledger reservation row.
    assert_eq!(provider.requests(), 1);
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, record.reservation_id);
    assert_eq!(rows[0].provider, "exa");
    assert_eq!(rows[0].max_cost_micros, 400);
    assert_eq!(rows[0].state, ReservationState::Settled);
    assert_eq!(rows[0].actual_micros, Some(250));
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        250
    );
}

#[tokio::test]
async fn reservation_refused_at_the_cap_skips_with_zero_provider_requests() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);

    // First call reserves 700 and keeps it open (delayed charge upstream).
    let first = MockProvider::new("exa", 700).with_behavior(Behavior::ReplyUnknown);
    assert!(matches!(
        gate.call(&first, String::from("q"), Duration::from_secs(5))
            .await,
        PaidOutcome::Ok { .. }
    ));

    // 400 on top of the committed 700 would pass the cap: refused.
    let second = MockProvider::new("parallel", 400);
    match gate
        .call(&second, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::SkippedBudget {
            provider,
            budget,
            requested,
            committed,
            cap,
        } => {
            assert_eq!(provider, "parallel");
            assert_eq!(budget, OPERATING_BUDGET);
            assert_eq!((requested, committed, cap), (400, 700, 1_000));
        }
        other => panic!("expected SkippedBudget, got {other:?}"),
    }

    // ZERO provider requests and zero rows for the refused call.
    assert_eq!(first.requests(), 1);
    assert_eq!(second.requests(), 0);
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].provider, "exa");
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        700
    );
}

/// Simulates a restart between reserve and settle: the gate task is killed and
/// the ledger connection closed, then the database is reopened. Abrupt
/// process-kill durability relies on SQLite WAL and is deliberately not tested
/// here.
#[tokio::test]
async fn restart_between_reserve_and_settle_keeps_the_liability() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let reservation_id;
    {
        let ledger = Arc::new(Ledger::open(&path).unwrap());
        ledger.set_budget(OPERATING_BUDGET, 1_000).unwrap();
        let gate = Arc::new(make_gate(Arc::clone(&ledger), OPERATING_BUDGET));
        let provider = Arc::new(MockProvider::new("exa", 600).with_behavior(Behavior::Hang));
        let task = {
            let gate = Arc::clone(&gate);
            let provider = Arc::clone(&provider);
            tokio::spawn(async move {
                gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                    .await
            })
        };
        // The provider has been dispatched, so the row was committed before
        // dispatch; nothing settled yet.
        provider.dispatched().notified().await;
        reservation_id = provider.reservation_ids()[0].clone();
        // Restart: kill the task and drop gate + provider + ledger (closing
        // the connection) below.
        task.abort();
        let _ = task.await;
    }

    // The committed liability survived the crash and still counts.
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, reservation_id);
    assert_eq!(rows[0].state, ReservationState::Open);
    assert_eq!(rows[0].actual_micros, None);
}

#[tokio::test]
async fn ambiguous_timeout_keeps_the_liability() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 600).with_behavior(Behavior::Hang);

    match gate
        .call(&provider, String::from("q"), Duration::from_millis(30))
        .await
    {
        PaidOutcome::Timeout { record } => {
            assert_eq!(record.provider, "exa");
            assert_eq!(record.max_cost_micros, 600);
            assert_eq!(record.charge, Charge::Estimated(600));
            assert_eq!(record.settlement, Settlement::Open);
        }
        other => panic!("expected Timeout, got {other:?}"),
    }

    assert_eq!(provider.requests(), 1);
    // Ambiguous: the request may have been billed upstream, so the liability
    // stays counted at its estimate.
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
    let open = ledger.open_reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, ReservationState::Open);
}

#[tokio::test]
async fn a_retry_needs_a_second_reservation() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::FailUnknown);

    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Failed { message, record } => {
            assert_eq!(message, "upstream failed");
            assert_eq!(record.charge, Charge::Estimated(400));
            assert_eq!(record.settlement, Settlement::Open);
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    // The controller retries by calling again: the gate has no automatic
    // retries, so a retry is a new reservation (one ticket = one attempt).
    provider.set_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 120,
    });
    assert!(matches!(
        gate.call(&provider, String::from("q"), Duration::from_secs(5))
            .await,
        PaidOutcome::Ok { .. }
    ));

    assert_eq!(provider.requests(), 2);
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].id, rows[1].id);
    // Each dispatch carried its own reservation id, matching its own row.
    let mut from_rows: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    from_rows.sort();
    let mut from_provider = provider.reservation_ids();
    from_provider.sort();
    assert_eq!(from_provider, from_rows);
    // The failed attempt's liability is still open at its estimate (400) and
    // the retry settled at its actual 120.
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        520
    );
}

#[tokio::test]
async fn failed_attempt_with_actual_charge_settles_at_that_charge() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::FailActual {
        message: "upstream 502",
        charge: 90,
    });

    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Failed { message, record } => {
            assert_eq!(message, "upstream 502");
            // The provider still billed this attempt, so the ticket settles at
            // the reported charge even though the call failed.
            assert_eq!(record.charge, Charge::Actual(90));
            assert_eq!(record.settlement, Settlement::Settled);
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    assert_eq!(provider.requests(), 1);
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ReservationState::Settled);
    assert_eq!(rows[0].actual_micros, Some(90));
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        90
    );
}

#[tokio::test]
async fn ledger_write_failure_dispatches_zero_provider_requests() {
    let fake = Arc::new(BrokenReserveLedger);
    let gate = PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap();
    let provider = MockProvider::new("exa", 400);

    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::LedgerUnavailable {
            provider: refused,
            error,
        } => {
            assert_eq!(refused, "exa");
            assert!(!error.is_empty());
        }
        other => panic!("expected LedgerUnavailable, got {other:?}"),
    }
    assert_eq!(provider.requests(), 0);
}

#[tokio::test]
async fn double_settle_is_counted_once() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 100,
    });

    assert!(matches!(
        gate.call(&provider, String::from("q"), Duration::from_secs(5))
            .await,
        PaidOutcome::Ok { .. }
    ));
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        100
    );

    // A second settle (e.g. a replayed late charge) must not count twice: the
    // receipt reports the authoritative first amount and flags the replay.
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ReservationState::Settled);
    let replay = ledger.settle_by_id(&rows[0].id, 900, now()).unwrap();
    assert_eq!(
        replay,
        SettleReceipt {
            recorded_micros: 100,
            already_settled: true,
        }
    );
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        100
    );
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows[0].actual_micros, Some(100));
}

#[tokio::test]
async fn cancellation_with_delayed_charge_settlement() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = Arc::new(make_gate(Arc::clone(&ledger), OPERATING_BUDGET));
    let release = Arc::new(Notify::new());
    let provider =
        Arc::new(MockProvider::new("exa", 600).with_behavior(Behavior::Wait(Arc::clone(&release))));
    let task = {
        let gate = Arc::clone(&gate);
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                .await
        })
    };
    provider.dispatched().notified().await;
    let reservation_id = provider.reservation_ids()[0].clone();
    task.abort();
    let join = task.await.unwrap_err();
    assert!(
        join.is_cancelled(),
        "expected a cancelled task, got {join:?}"
    );

    // Cancellation released nothing: the row is open and counted at its
    // estimate (there is no Drop impl that could free the liability).
    let open = ledger.open_reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, reservation_id);
    assert_eq!(open[0].state, ReservationState::Open);
    assert_eq!(provider.requests(), 1);
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );

    // The delayed charge settles by id at the real amount.
    ledger
        .settle_by_id(&reservation_id, 120, now() + 30)
        .unwrap();
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        120
    );
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows[0].state, ReservationState::Settled);
    assert_eq!(rows[0].actual_micros, Some(120));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_reservations_at_the_cap_dispatch_exactly_one_request() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let exa = MockProvider::new("exa", 600).with_behavior(Behavior::ReplyUnknown);
    let parallel = MockProvider::new("parallel", 600).with_behavior(Behavior::ReplyUnknown);

    // Both reserve at the same time; each wants 600 of a 1_000 cap. Neither
    // reports a charge, so the winner's liability stays at its 600 estimate.
    let (a, b) = tokio::join!(
        gate.call(&exa, String::from("a"), Duration::from_secs(5)),
        gate.call(&parallel, String::from("b"), Duration::from_secs(5)),
    );

    let (mut wins, mut skips) = (0, 0);
    for outcome in [a, b] {
        match outcome {
            PaidOutcome::Ok { record, .. } => {
                wins += 1;
                assert_eq!(record.charge, Charge::Estimated(600));
                assert_eq!(record.settlement, Settlement::Open);
            }
            PaidOutcome::SkippedBudget {
                requested,
                committed,
                cap,
                ..
            } => {
                skips += 1;
                // The loser saw the winner's committed row inside its own
                // reservation transaction: the cap is a hard limit.
                assert_eq!((requested, committed, cap), (600, 600, 1_000));
            }
            other => panic!("expected Ok or SkippedBudget, got {other:?}"),
        }
    }
    assert_eq!((wins, skips), (1, 1));

    // No overspend: exactly one provider request and one ledger row.
    assert_eq!(exa.requests() + parallel.requests(), 1);
    assert_eq!(ledger.reservations(OPERATING_BUDGET).unwrap().len(), 1);
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
}

#[tokio::test]
async fn eval_and_operating_gates_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    ledger.set_budget(OPERATING_BUDGET, 1_000).unwrap();
    ledger.set_budget(EVAL_BUDGET, 100).unwrap();
    let operating = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let eval = make_gate(Arc::clone(&ledger), EVAL_BUDGET);
    let reference = MockProvider::new("exa", 100).with_behavior(Behavior::ReplyActual {
        body: "baseline",
        charge: 100,
    });
    let search = MockProvider::new("parallel", 400).with_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 300,
    });

    // Eval exhausts its own budget...
    assert!(matches!(
        eval.call(&reference, String::from("q"), Duration::from_secs(5))
            .await,
        PaidOutcome::Ok { .. }
    ));
    match eval
        .call(&reference, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::SkippedBudget { budget, cap, .. } => {
            assert_eq!(budget, EVAL_BUDGET);
            assert_eq!(cap, 100);
        }
        other => panic!("eval must be out of budget, got {other:?}"),
    }
    assert_eq!(reference.requests(), 1);

    // ...while the operating gate still dispatches against its own cap.
    match operating
        .call(&search, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Ok { record, .. } => {
            assert_eq!(record.budget, OPERATING_BUDGET);
            assert_eq!(record.charge, Charge::Actual(300));
        }
        other => panic!("operating must still dispatch, got {other:?}"),
    }
    assert_eq!(search.requests(), 1);
    assert_eq!(ledger.committed_micros(EVAL_BUDGET, period()).unwrap(), 100);
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        300
    );
}

#[tokio::test]
async fn provider_sees_the_reservation_id_of_the_ledger_row() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 300).with_behavior(Behavior::ReplyUnknown);

    let record = match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Ok { record, .. } => record,
        other => panic!("expected Ok, got {other:?}"),
    };

    // The id the provider used as its idempotency key IS the row's id, and the
    // disclosure record carries the same id.
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(provider.reservation_ids(), vec![rows[0].id.clone()]);
    assert_eq!(record.reservation_id, rows[0].id);
}

#[tokio::test]
async fn settle_failure_returns_body_and_defers_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    inner.set_budget(OPERATING_BUDGET, 1_000).unwrap();
    let fake = Arc::new(BrokenSettleLedger {
        inner: Arc::clone(&inner),
    });
    let gate = PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap();
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 150,
    });

    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Ok { body, record } => {
            // Never lose the body.
            assert_eq!(body, "evidence");
            // The books count the estimate until reconciliation, so the
            // disclosure reports the estimate with a deferred settlement that
            // keeps the reported charge for later.
            assert_eq!(record.charge, Charge::Estimated(400));
            assert_eq!(record.over_bound_micros, None);
            match record.settlement {
                Settlement::Deferred {
                    error,
                    reported_micros,
                } => {
                    assert!(!error.is_empty());
                    assert_eq!(reported_micros, 150);
                }
                other => panic!("expected Deferred, got {other:?}"),
            }
        }
        other => panic!("expected Ok, got {other:?}"),
    }

    // The row is still open and counted at its estimate...
    let open = inner.open_reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, ReservationState::Open);
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        400
    );
    // ...and a later reconciliation by id records the real charge.
    inner.settle_by_id(&open[0].id, 150, now()).unwrap();
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        150
    );
}

#[tokio::test]
async fn unknown_budget_fails_closed_with_zero_requests() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    // A gate pointed at a budget nobody configured must fail closed.
    let gate = make_gate(Arc::clone(&ledger), "ghost");
    let provider = MockProvider::new("exa", 100);

    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::LedgerUnavailable { provider: p, error } => {
            assert_eq!(p, "exa");
            assert!(error.contains("ghost"), "unexpected error: {error}");
        }
        other => panic!("expected LedgerUnavailable, got {other:?}"),
    }
    assert_eq!(provider.requests(), 0);
}

#[tokio::test]
async fn restart_keeps_unknown_charge_liability_and_expiry_charges_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let reservation_id;
    {
        let ledger = Arc::new(Ledger::open(&path).unwrap());
        ledger.set_budget(OPERATING_BUDGET, 1_000).unwrap();
        let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
        let provider = MockProvider::new("exa", 600).with_behavior(Behavior::ReplyUnknown);
        reservation_id = match gate
            .call(&provider, String::from("q"), Duration::from_secs(5))
            .await
        {
            PaidOutcome::Ok { record, .. } => {
                assert_eq!(record.charge, Charge::Estimated(600));
                assert_eq!(record.settlement, Settlement::Open);
                record.reservation_id
            }
            other => panic!("expected Ok, got {other:?}"),
        };
    }

    // Restart: the possibly-charged attempt is still on the books.
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, reservation_id);
    assert_eq!(rows[0].state, ReservationState::Open);

    // Configured expiry ages the row but keeps charging the estimate.
    assert_eq!(ledger.expire_open_older_than(1, now() + 10).unwrap(), 1);
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
    assert_eq!(
        ledger.reservations(OPERATING_BUDGET).unwrap()[0].state,
        ReservationState::Expired
    );

    // A late charge can still settle it at the real amount.
    ledger
        .settle_by_id(&reservation_id, 50, now() + 11)
        .unwrap();
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        50
    );
}

#[test]
fn gate_rejects_an_empty_budget_name() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let result = PaidGate::new(ledger, "", Arc::new(FixedClock(now())));
    match result {
        Err(LedgerError::InvalidArgument(msg)) => assert!(!msg.is_empty()),
        Err(other) => panic!("expected InvalidArgument, got {other:?}"),
        Ok(_) => panic!("an empty budget name must be rejected"),
    }
}

#[tokio::test]
async fn a_ticket_that_does_not_match_the_request_fails_closed() {
    // Defense in depth behind the sealed ledger: even an in-crate bug that
    // hands back a real ticket minted for a different provider or amount must
    // not lead to a dispatch. The mismatched row stays counted and is never
    // settled by the gate.
    for mismatch in [Mismatch::Provider, Mismatch::Amount] {
        let dir = tempfile::tempdir().unwrap();
        let inner = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
        inner.set_budget(OPERATING_BUDGET, 1_000).unwrap();
        let fake = Arc::new(MismatchedTicketLedger {
            inner: Arc::clone(&inner),
            mismatch,
        });
        let gate = PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap();
        let provider = MockProvider::new("exa", 400);

        match gate
            .call(&provider, String::from("q"), Duration::from_secs(5))
            .await
        {
            PaidOutcome::LedgerUnavailable {
                provider: refused,
                error,
            } => {
                assert_eq!(refused, "exa");
                assert!(!error.is_empty());
            }
            other => panic!("mismatch {mismatch:?} must fail closed, got {other:?}"),
        }

        // ZERO provider requests, and the mismatched row is untouched: open
        // and counted at the amount the fake actually reserved.
        assert_eq!(provider.requests(), 0);
        let (expected_provider, expected_max) = match mismatch {
            Mismatch::Provider => ("someone-else", 400),
            Mismatch::Amount => ("exa", 401),
        };
        let rows = inner.reservations(OPERATING_BUDGET).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, expected_provider);
        assert_eq!(rows[0].max_cost_micros, expected_max);
        assert_eq!(rows[0].state, ReservationState::Open);
        assert_eq!(
            inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
            expected_max
        );
    }
}

#[tokio::test]
async fn a_racing_reconciliation_reports_the_already_recorded_charge() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = Arc::new(make_gate(Arc::clone(&ledger), OPERATING_BUDGET));
    let release = Arc::new(Notify::new());
    let provider = Arc::new(MockProvider::new("exa", 400).with_behavior(
        Behavior::WaitThenActual {
            release: Arc::clone(&release),
            charge: 250,
        },
    ));
    let task = {
        let gate = Arc::clone(&gate);
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                .await
        })
    };

    // While the provider is still working, a reconciler settles the row at 100.
    provider.dispatched().notified().await;
    let reservation_id = provider.reservation_ids()[0].clone();
    let first = ledger.settle_by_id(&reservation_id, 100, now()).unwrap();
    assert_eq!(
        first,
        SettleReceipt {
            recorded_micros: 100,
            already_settled: false,
        }
    );

    // Now the provider answers with a different actual (250): the first
    // charge wins, and the record discloses both amounts instead of losing
    // the reported one.
    release.notify_one();
    let record = match task.await.unwrap() {
        PaidOutcome::Ok { body, record } => {
            assert_eq!(body, "released");
            record
        }
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_eq!(
        record.settlement,
        Settlement::AlreadySettled {
            recorded_micros: 100,
            reported_micros: 250,
        }
    );
    assert_eq!(record.charge, Charge::Actual(100));
    assert_eq!(record.over_bound_micros, None);
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        100
    );
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows[0].state, ReservationState::Settled);
    assert_eq!(rows[0].actual_micros, Some(100));
}

#[tokio::test]
async fn an_equal_amount_replay_is_still_disclosed_as_already_settled() {
    // Same race as above, but the reconciler and the provider agree on the
    // amount. The gate's own settle wrote nothing, so the record must still
    // say the row was already settled rather than claim it settled it.
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = Arc::new(make_gate(Arc::clone(&ledger), OPERATING_BUDGET));
    let release = Arc::new(Notify::new());
    let provider = Arc::new(MockProvider::new("exa", 400).with_behavior(
        Behavior::WaitThenActual {
            release: Arc::clone(&release),
            charge: 250,
        },
    ));
    let task = {
        let gate = Arc::clone(&gate);
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                .await
        })
    };

    provider.dispatched().notified().await;
    let reservation_id = provider.reservation_ids()[0].clone();
    ledger.settle_by_id(&reservation_id, 250, now()).unwrap();
    release.notify_one();

    let record = match task.await.unwrap() {
        PaidOutcome::Ok { record, .. } => record,
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_eq!(
        record.settlement,
        Settlement::AlreadySettled {
            recorded_micros: 250,
            reported_micros: 250,
        }
    );
    assert_eq!(record.charge, Charge::Actual(250));
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        250
    );
}

#[tokio::test]
async fn above_bound_actual_is_recorded_as_is_and_disclosed() {
    // The provider's code-set bound is wrong (it billed more than it said it
    // could): the charge is NOT clamped, the ledger records the real amount
    // and the record discloses the bound violation.
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = make_gate(Arc::clone(&ledger), OPERATING_BUDGET);
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 450,
    });

    let record = match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Ok { body, record } => {
            assert_eq!(body, "evidence");
            record
        }
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_eq!(record.charge, Charge::Actual(450));
    assert_eq!(record.settlement, Settlement::Settled);
    assert_eq!(record.over_bound_micros, Some(50));

    // The books record the full actual, not the bound.
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows[0].state, ReservationState::Settled);
    assert_eq!(rows[0].actual_micros, Some(450));
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        450
    );
}

#[tokio::test]
async fn above_bound_actual_with_failing_settlement_keeps_the_reported_charge() {
    // Same bound violation, but the settlement write fails: the deferred
    // settlement carries the reported actual so nothing is discarded, the
    // bound violation is still disclosed, and the books undercount by
    // over_bound (400 counted vs 450 real) until reconciliation.
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    inner.set_budget(OPERATING_BUDGET, 1_000).unwrap();
    let fake = Arc::new(BrokenSettleLedger {
        inner: Arc::clone(&inner),
    });
    let gate = PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap();
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 450,
    });

    let record = match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Ok { body, record } => {
            assert_eq!(body, "evidence");
            record
        }
        other => panic!("expected Ok, got {other:?}"),
    };
    assert_eq!(record.charge, Charge::Estimated(400));
    assert_eq!(record.over_bound_micros, Some(50));
    match record.settlement {
        Settlement::Deferred {
            error,
            reported_micros,
        } => {
            assert!(!error.is_empty());
            assert_eq!(reported_micros, 450);
        }
        other => panic!("expected Deferred, got {other:?}"),
    }

    // Row open at its estimate until a later reconciliation records 450.
    let open = inner.open_reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, ReservationState::Open);
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        400
    );
    inner.settle_by_id(&open[0].id, 450, now()).unwrap();
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        450
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_during_reservation_keeps_the_committed_row_counted() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    inner.set_budget(OPERATING_BUDGET, 1_000).unwrap();
    let (park, controls) = parking();
    let fake = Arc::new(BlockingReserveLedger {
        inner: Arc::clone(&inner),
        park,
    });
    let gate =
        Arc::new(PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap());
    let provider = Arc::new(MockProvider::new("exa", 600));
    let task = {
        let gate = Arc::clone(&gate);
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                .await
        })
    };

    // The row is committed before the reservation returns; wait until the
    // fake is parked mid-reserve, then kill the caller.
    controls.entered.recv().unwrap();
    task.abort();
    let join = task.await.unwrap_err();
    assert!(
        join.is_cancelled(),
        "expected a cancelled task, got {join:?}"
    );

    // Release the parked reserve; its ticket is dropped unused.
    controls.release.send(()).unwrap();
    controls.finished.recv().unwrap();

    // Nothing was dispatched and nothing was released: the committed row is
    // open and counted at its estimate.
    assert_eq!(provider.requests(), 0);
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
    let rows = inner.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ReservationState::Open);
    assert_eq!(rows[0].actual_micros, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_during_settlement_still_records_the_charge() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    inner.set_budget(OPERATING_BUDGET, 1_000).unwrap();
    let (park, controls) = parking();
    let fake = Arc::new(BlockingSettleLedger {
        inner: Arc::clone(&inner),
        park,
    });
    let gate =
        Arc::new(PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap());
    let provider = Arc::new(
        MockProvider::new("exa", 400).with_behavior(Behavior::ReplyActual {
            body: "evidence",
            charge: 250,
        }),
    );
    let task = {
        let gate = Arc::clone(&gate);
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                .await
        })
    };

    // The provider answered and the settlement is parked before it writes;
    // kill the caller mid-settlement.
    controls.entered.recv().unwrap();
    task.abort();
    let join = task.await.unwrap_err();
    assert!(
        join.is_cancelled(),
        "expected a cancelled task, got {join:?}"
    );

    // Release the parked settlement: spawn_blocking work runs to completion
    // even though its caller is gone, so the charge must still land.
    controls.release.send(()).unwrap();
    controls.finished.recv().unwrap();

    let mut settled = false;
    for _ in 0..500 {
        let rows = inner.reservations(OPERATING_BUDGET).unwrap();
        if rows[0].state == ReservationState::Settled {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        settled,
        "the detached settlement must finish and settle the row"
    );
    let rows = inner.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows[0].actual_micros, Some(250));
    assert_eq!(provider.requests(), 1);
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        250
    );
}

#[tokio::test]
async fn reserve_join_panic_fails_closed_without_dispatch() {
    let fake = Arc::new(PanicReserveLedger);
    let gate = PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap();
    let provider = MockProvider::new("exa", 400);

    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::LedgerUnavailable {
            provider: refused,
            error,
        } => {
            assert_eq!(refused, "exa");
            assert!(!error.is_empty());
        }
        other => panic!("expected LedgerUnavailable, got {other:?}"),
    }
    assert_eq!(provider.requests(), 0);
}

#[tokio::test]
async fn settle_join_panic_defers_with_the_reported_charge() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Ledger::open(dir.path().join("ledger.db")).unwrap());
    inner.set_budget(OPERATING_BUDGET, 1_000).unwrap();
    let fake = Arc::new(PanicSettleLedger {
        inner: Arc::clone(&inner),
    });
    let gate = PaidGate::new(fake, OPERATING_BUDGET, Arc::new(FixedClock(now()))).unwrap();
    let provider = MockProvider::new("exa", 400).with_behavior(Behavior::ReplyActual {
        body: "evidence",
        charge: 150,
    });

    // The body comes back with a deferred settlement that keeps the reported
    // charge; the estimate stays counted.
    match gate
        .call(&provider, String::from("q"), Duration::from_secs(5))
        .await
    {
        PaidOutcome::Ok { body, record } => {
            assert_eq!(body, "evidence");
            assert_eq!(record.charge, Charge::Estimated(400));
            assert_eq!(record.over_bound_micros, None);
            match record.settlement {
                Settlement::Deferred {
                    error,
                    reported_micros,
                } => {
                    assert!(!error.is_empty());
                    assert_eq!(reported_micros, 150);
                }
                other => panic!("expected Deferred, got {other:?}"),
            }
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    let open = inner.open_reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, ReservationState::Open);
    assert_eq!(
        inner.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        400
    );
}

#[tokio::test]
async fn provider_panic_leaves_the_liability_open() {
    let (_dir, ledger) = setup(OPERATING_BUDGET, 1_000);
    let gate = Arc::new(make_gate(Arc::clone(&ledger), OPERATING_BUDGET));
    let provider = Arc::new(MockProvider::new("exa", 600).with_behavior(Behavior::Panic));
    let task = {
        let gate = Arc::clone(&gate);
        let provider = Arc::clone(&provider);
        tokio::spawn(async move {
            gate.call(&*provider, String::from("q"), Duration::from_secs(3600))
                .await
        })
    };

    // The panic surfaces as a panicked join error on the gate task.
    let join = task.await.unwrap_err();
    assert!(join.is_panic(), "expected a panicked task, got {join:?}");

    // The row was committed before the dispatch and stays open, counted at
    // its estimate.
    assert_eq!(provider.requests(), 1);
    assert_eq!(
        ledger.committed_micros(OPERATING_BUDGET, period()).unwrap(),
        600
    );
    let rows = ledger.reservations(OPERATING_BUDGET).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, ReservationState::Open);
    assert_eq!(rows[0].actual_micros, None);
}
