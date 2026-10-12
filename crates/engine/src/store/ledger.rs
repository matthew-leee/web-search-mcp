//! Durable budget ledger for paid calls: reservations, settlement and monthly caps.
//!
//! Contract (decisions D6 and D13, design doc :51): before any paid attempt the
//! caller reserves the maximum expected cost against a named budget; the issued
//! [`Reservation`] is the only ticket the paid gate accepts, and it is consumed
//! when the real charge is settled.
//!
//! Why this module is shaped the way it is:
//!
//! - **Fail closed.** The reservation row is committed to SQLite *before* the
//!   ticket exists, so a caller never holds a ticket the ledger has not durably
//!   recorded. If the write fails, [`Ledger::reserve`] returns
//!   [`LedgerError::Storage`] and nothing is dispatched (D13: "ledger-write
//!   failure means no dispatch").
//! - **Liabilities survive restart and cancellation.** An unfinished row keeps
//!   counting at its `max_cost` estimate after a crash, restart or cancellation;
//!   dropping the ticket releases nothing (there is deliberately no `Drop`
//!   impl). An ambiguous timeout therefore cannot make spend disappear from the
//!   books.
//! - **One ticket = one physical attempt.** A retry is a new attempt and must
//!   reserve again, so a retry storm cannot silently multiply spend past the cap.
//! - **Money is integer micro-USD ([`Micros`]).** Floats round, and rounding
//!   drift must never eat into (or exceed) the cap.
//! - **The monthly cap is enforced per UTC calendar month** (the `period`
//!   column, derived from `created_at` in UTC) inside one `BEGIN IMMEDIATE`
//!   transaction, so concurrent reservations at the cap cannot both win.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use time::OffsetDateTime;
use uuid::Uuid;

/// Money in integer micro-USD (one millionth of one US dollar).
///
/// Integer arithmetic only: the ledger must never round, so it must never see a
/// float.
pub type Micros = i64;

/// Schema version recorded in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

/// How long a writer waits for a competing SQLite writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause between retries of the WAL journal-mode switch; see
/// [`set_wal_journal_mode`].
const JOURNAL_MODE_RETRY_PAUSE: Duration = Duration::from_millis(10);

/// Schema for a fresh database. Migrations append here as `SCHEMA_VERSION` grows.
const CREATE_SCHEMA_SQL: &str = "
    CREATE TABLE budgets (
        name TEXT PRIMARY KEY,
        monthly_cap_micros INTEGER NOT NULL
    );
    CREATE TABLE reservations (
        id TEXT PRIMARY KEY,
        budget TEXT NOT NULL,
        provider TEXT NOT NULL,
        max_cost_micros INTEGER NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('open', 'settled', 'expired')),
        actual_micros INTEGER,
        created_at INTEGER NOT NULL,
        settled_at INTEGER,
        period TEXT NOT NULL
    );
    CREATE INDEX idx_reservations_budget_period ON reservations (budget, period);
";

/// Errors from the budget ledger.
///
/// The cap path is deliberately loud ([`LedgerError::OverCap`]): refusing a paid
/// dispatch must be distinguishable from a storage outage at the call site.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The reservation would push the budget's committed spend for the period
    /// past its monthly cap. Nothing was written.
    #[error(
        "budget {budget} is over cap: committed {committed} + requested {requested} exceeds cap {cap}"
    )]
    OverCap {
        /// Budget the reservation was attempted against.
        budget: String,
        /// Configured monthly cap in micro-USD.
        cap: Micros,
        /// Already-committed spend for the current period in micro-USD.
        committed: Micros,
        /// Rejected reservation's maximum cost in micro-USD.
        requested: Micros,
    },
    /// The named budget has no configured cap.
    #[error("unknown budget {0}")]
    UnknownBudget(String),
    /// No reservation row with this id exists (typo or already pruned).
    #[error("unknown reservation {0}")]
    UnknownReservation(String),
    /// A money value is out of range (non-positive reservation, negative charge
    /// or negative cap).
    #[error("invalid amount: {0}")]
    InvalidAmount(String),
    /// A non-money argument is malformed (bad period string, bad timestamp,
    /// empty name).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The database was written by a newer (or much older) build.
    #[error("unsupported ledger schema version {found} (this build supports {supported})")]
    SchemaVersion {
        /// Version found in the database.
        found: i64,
        /// Version this build supports.
        supported: i64,
    },
    /// The ledger lives on storage that cannot be durable (WAL could not be
    /// enabled). Fail closed like [`LedgerError::Storage`]: a ledger that
    /// vanishes on restart cannot cap spend.
    #[error("ledger storage is not durable: {0}")]
    NotDurable(String),
    /// A row violates the reservation state machine; treat the data as suspect.
    #[error("ledger row is corrupt: {0}")]
    Corrupt(String),
    /// SQLite could not read or write the ledger. Fail closed: no ticket was
    /// issued, so no paid call may be dispatched.
    #[error("ledger storage failure: {0}")]
    Storage(#[from] rusqlite::Error),
}

/// State of a reservation row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationState {
    /// Reserved and awaiting settlement; counts at its `max_cost` estimate.
    Open,
    /// Aged past the configured expiry and charged at its `max_cost` estimate
    /// until a late settlement reports the actual charge.
    Expired,
    /// Settlement recorded the attempt. Counts at its `actual_micros` when the
    /// settlement reported one, otherwise at its `max_cost` estimate.
    Settled,
}

/// Read-only snapshot of a reservation row, in any state.
///
/// This is an inspection record, not a dispatch ticket: only [`Ledger::reserve`]
/// can issue the [`Reservation`] the paid gate accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationInfo {
    /// Unique reservation id (matches [`Reservation::id`]).
    pub id: String,
    /// Provider the attempt was reserved for.
    pub provider: String,
    /// Maximum cost reserved, in micro-USD.
    pub max_cost_micros: Micros,
    /// Actual charge recorded at settlement, in micro-USD.
    ///
    /// `None` while the row is unsettled (`open` or `expired`) and for a
    /// settlement that reported no amount; in both cases the row counts at its
    /// `max_cost_micros` estimate instead.
    pub actual_micros: Option<Micros>,
    /// Whether the row is still `open`, was aged out to `expired`, or is
    /// `settled`.
    pub state: ReservationState,
    /// Creation time as Unix seconds (UTC).
    pub created_at_unix: i64,
    /// UTC accounting period (`YYYY-MM`) derived from `created_at_unix`.
    pub period: String,
}

/// What a settlement actually recorded on the reservation row.
///
/// The recorded charge is authoritative: on an idempotent replay the first
/// settlement's amount stands and is reported here, so a caller always learns
/// what the books count instead of echoing back what it tried to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleReceipt {
    /// The actual charge now on the row, in micro-USD: the first settlement's
    /// amount wins on an idempotent replay; a settled row without a recorded
    /// actual reports its `max_cost` estimate (what it counts as on the books).
    pub recorded_micros: Micros,
    /// `true` when the row was already settled before this call (idempotent
    /// replay): `recorded_micros` is the earlier amount, not this call's.
    pub already_settled: bool,
}

/// A budget reservation ticket: proof that the cost of one paid attempt is
/// durably counted in the [`Ledger`].
///
/// Fields are private and there is no public constructor, so only this module
/// can mint one. It is deliberately neither `Clone` nor `Copy`: one ticket
/// stands for exactly one physical attempt, and it is consumed by
/// [`Ledger::settle`]. There is deliberately no [`Drop`] impl either — dropping
/// the ticket leaves the row `open`, so the liability keeps counting until it is
/// settled or expires. A crash or cancellation can therefore never make spend
/// disappear from the books (decision D13).
///
/// Those invariants are compile-time properties of the type, so they are
/// protected by negative tests. A ticket cannot be forged outside this crate:
/// its fields are private and only [`Ledger::reserve`] constructs one.
///
/// ```compile_fail,E0451
/// use engine::store::ledger::Reservation;
///
/// fn main() {
///     // Fails with E0451: the fields of `Reservation` are private.
///     let _forged = Reservation {
///         id: String::from("forged"),
///         provider: String::from("exa"),
///         max_cost_micros: 1_000,
///     };
/// }
/// ```
///
/// And it cannot be duplicated to smuggle two attempts past one reservation:
/// [`Reservation`] deliberately does not implement [`Clone`].
///
/// ```compile_fail,E0277
/// use engine::store::ledger::Reservation;
///
/// fn requires_clone<T: Clone>(ticket: &T) {
///     let _ = ticket.clone();
/// }
///
/// fn example(ticket: &Reservation) {
///     // Fails with E0277: the trait bound `Reservation: Clone` is not satisfied.
///     requires_clone(ticket);
/// }
///
/// fn main() {}
/// ```
#[derive(Debug)]
pub struct Reservation {
    id: String,
    provider: String,
    max_cost_micros: Micros,
}

impl Reservation {
    /// Unique id of the backing reservation row.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Provider this ticket was reserved for.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Maximum cost reserved for this attempt, in micro-USD.
    pub fn max_cost_micros(&self) -> Micros {
        self.max_cost_micros
    }
}

/// Durable budget ledger backed by a SQLite database file.
///
/// Every operation is atomic and fail-closed; open one per database file and
/// share it behind `&self` (the engine daemon is the sole owner, decision D3).
#[derive(Debug)]
pub struct Ledger {
    // `BEGIN IMMEDIATE` inside each operation is what makes the cap a hard limit
    // under concurrency; the mutex just serializes callers on this connection.
    conn: Mutex<Connection>,
}

impl Ledger {
    /// Open (or create) the ledger database at `path` and migrate its schema.
    ///
    /// The path must be a durable database file. Empty paths and in-memory
    /// databases (`":memory:"`, `file::memory:...`, anything with
    /// `mode=memory`) are rejected with [`LedgerError::InvalidArgument`]
    /// *before* the connection is opened: an in-memory ledger loses every
    /// liability on restart and gives each connection its own private caps.
    /// After the journal mode is set to WAL, the reported mode must actually be
    /// `wal`; anything else is refused with [`LedgerError::NotDurable`].
    ///
    /// Reopening an existing database is idempotent: the schema is created only
    /// when `PRAGMA user_version` is still 0 read inside the migration
    /// transaction, and newer versions are refused with
    /// [`LedgerError::SchemaVersion`]. WAL mode keeps readers from blocking
    /// writers; the cap's correctness comes from the `BEGIN IMMEDIATE`
    /// transactions, not from WAL.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref();
        let raw = path.to_string_lossy();
        let lowered = raw.to_ascii_lowercase();
        if lowered.is_empty() {
            return Err(LedgerError::InvalidArgument(
                "ledger path must not be empty: the ledger must be a durable database file"
                    .to_owned(),
            ));
        }
        if lowered == ":memory:"
            || lowered.starts_with("file::memory")
            || lowered.contains("mode=memory")
        {
            return Err(LedgerError::InvalidArgument(format!(
                "ledger path {raw:?} points at an in-memory database: the ledger must be a durable file so liabilities survive restarts"
            )));
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Must run before any other statement touches the database: WAL is the
        // journal mode this design assumes. Errors here (including a read-only
        // database) surface as Storage, and a database that cannot actually run
        // in WAL as NotDurable; both fail closed.
        let mode = set_wal_journal_mode(&mut conn)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(LedgerError::NotDurable(format!(
                "journal_mode is {mode:?}, expected \"wal\": liabilities must survive restarts"
            )));
        }
        Self::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn migrate(conn: &mut Connection) -> Result<(), LedgerError> {
        // The user_version read and the CREATE decision run inside one
        // BEGIN IMMEDIATE transaction: two concurrent first opens of a fresh
        // file must not both see 0 and then collide on CREATE TABLE. The
        // transaction is committed unchanged when the schema is current.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: i64 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match found {
            0 => {
                tx.execute_batch(CREATE_SCHEMA_SQL)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
                Ok(())
            }
            SCHEMA_VERSION => {
                tx.commit()?;
                Ok(())
            }
            found => Err(LedgerError::SchemaVersion {
                found,
                supported: SCHEMA_VERSION,
            }),
        }
    }

    /// Create or update a budget's monthly cap in micro-USD.
    ///
    /// A cap of `0` is allowed and acts as an explicit kill switch: every
    /// reservation then fails with [`LedgerError::OverCap`]. A negative cap is a
    /// caller bug and rejected.
    pub fn set_budget(&self, name: &str, monthly_cap_micros: Micros) -> Result<(), LedgerError> {
        if name.is_empty() {
            return Err(LedgerError::InvalidArgument(
                "budget name must not be empty".to_owned(),
            ));
        }
        if monthly_cap_micros < 0 {
            return Err(LedgerError::InvalidAmount(format!(
                "monthly_cap_micros must be >= 0, got {monthly_cap_micros}"
            )));
        }
        let conn = self.lock();
        conn.execute(
            "INSERT INTO budgets (name, monthly_cap_micros) VALUES (?1, ?2)
             ON CONFLICT (name) DO UPDATE SET monthly_cap_micros = excluded.monthly_cap_micros",
            params![name, monthly_cap_micros],
        )?;
        Ok(())
    }

    /// Reserve the maximum expected cost of one physical attempt against
    /// `budget`, returning the [`Reservation`] ticket the paid gate requires.
    ///
    /// Runs in a single `BEGIN IMMEDIATE` transaction so the cap check and the
    /// insert cannot interleave with another reservation. Committed spend for
    /// the budget's current UTC period is `max_cost` of `open` rows + `actual`
    /// of `settled` rows + `max_cost` of `expired` rows; the reservation is
    /// refused with [`LedgerError::OverCap`] (writing nothing) when
    /// `committed + max_cost > cap`.
    ///
    /// The row is committed **before** the ticket is handed back: fail closed,
    /// a caller never holds a ticket the ledger has not durably recorded. A
    /// retry must call this again (one ticket per physical attempt).
    pub fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost_micros: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        if max_cost_micros <= 0 {
            return Err(LedgerError::InvalidAmount(format!(
                "max_cost_micros must be > 0, got {max_cost_micros}"
            )));
        }
        if provider.is_empty() {
            return Err(LedgerError::InvalidArgument(
                "provider must not be empty".to_owned(),
            ));
        }
        let period = period_of(now_unix)?;

        // On drop the transaction rolls back, so every early return writes nothing.
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let cap: Micros = tx
            .query_row(
                "SELECT monthly_cap_micros FROM budgets WHERE name = ?1",
                params![budget],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| LedgerError::UnknownBudget(budget.to_owned()))?;

        let committed = committed_in(&tx, budget, &period)?;
        // i128 comparison: overflow must fail closed, not wrap under the cap.
        if i128::from(committed) + i128::from(max_cost_micros) > i128::from(cap) {
            return Err(LedgerError::OverCap {
                budget: budget.to_owned(),
                cap,
                committed,
                requested: max_cost_micros,
            });
        }

        let id = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO reservations
                (id, budget, provider, max_cost_micros, state, actual_micros, created_at, settled_at, period)
             VALUES (?1, ?2, ?3, ?4, 'open', NULL, ?5, NULL, ?6)",
            params![id, budget, provider, max_cost_micros, now_unix, period],
        )?;
        // Durability point: only after this commit does the ticket exist.
        tx.commit()?;

        Ok(Reservation {
            id,
            provider: provider.to_owned(),
            max_cost_micros,
        })
    }

    /// Settle the attempt a [`Reservation`] ticket stands for, consuming the
    /// ticket and recording the actual charge in micro-USD. The returned
    /// [`SettleReceipt`] reports the authoritative charge now on the row.
    ///
    /// Reconciliation rules:
    /// - the first settlement wins; settling an already-settled id again is a
    ///   no-op whose receipt reports `already_settled` and the first recorded
    ///   amount (idempotent double-settle must be counted once);
    /// - settling an `expired` row records the actual charge and settles it;
    /// - an `actual_micros` above the reserved maximum is recorded as-is: the
    ///   ledger reports reality even though the cap was enforced on the estimate.
    ///
    /// On error the ticket is consumed but no row changes; [`Ledger::settle_by_id`]
    /// can still reconcile the attempt later.
    pub fn settle(
        &self,
        reservation: Reservation,
        actual_micros: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        self.settle_by_id(&reservation.id, actual_micros, now_unix)
    }

    /// Settle a reservation by id, for restart and late-charge reconciliation
    /// when the original ticket no longer exists. The returned
    /// [`SettleReceipt`] reports the authoritative charge now on the row.
    ///
    /// Idempotent with the same rules as [`Ledger::settle`]: an idempotent
    /// replay reports `already_settled` and keeps the first recorded charge;
    /// unknown ids are rejected so a silent no-op cannot hide a lost charge.
    pub fn settle_by_id(
        &self,
        id: &str,
        actual_micros: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        if actual_micros < 0 {
            return Err(LedgerError::InvalidAmount(format!(
                "actual_micros must be >= 0, got {actual_micros}"
            )));
        }
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let changed = tx.execute(
            "UPDATE reservations
             SET state = 'settled', actual_micros = ?2, settled_at = ?3
             WHERE id = ?1 AND state != 'settled'",
            params![id, actual_micros, now_unix],
        )?;
        let receipt = if changed == 0 {
            let known: Option<(String, Option<Micros>, Micros)> = tx
                .query_row(
                    "SELECT state, actual_micros, max_cost_micros FROM reservations WHERE id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            match known {
                None => return Err(LedgerError::UnknownReservation(id.to_owned())),
                // Already settled: the first recorded charge stands. A settled
                // row without a recorded actual reports (and counts) its
                // estimate, matching `committed_in`.
                Some((state, actual, max_cost)) => match parse_state(&state)? {
                    ReservationState::Settled => SettleReceipt {
                        recorded_micros: actual.unwrap_or(max_cost),
                        already_settled: true,
                    },
                    other => {
                        return Err(LedgerError::Corrupt(format!(
                            "settle changed no row for reservation {id} but its state is {other:?}"
                        )));
                    }
                },
            }
        } else {
            SettleReceipt {
                recorded_micros: actual_micros,
                already_settled: false,
            }
        };
        tx.commit()?;
        Ok(receipt)
    }

    /// Mark `open` rows older than `max_age_secs` as `expired`, charging each at
    /// its `max_cost` estimate, and return how many rows were expired.
    ///
    /// Expiry never deletes or settles a row: the liability stays counted at the
    /// estimate until a late settlement reports the real charge, so time alone
    /// cannot shrink the books. `max_age_secs <= 0` expires every open row.
    pub fn expire_open_older_than(
        &self,
        max_age_secs: i64,
        now_unix: i64,
    ) -> Result<usize, LedgerError> {
        let cutoff = now_unix.saturating_sub(max_age_secs);
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE reservations SET state = 'expired'
             WHERE state = 'open' AND created_at < ?1",
            params![cutoff],
        )?;
        Ok(changed)
    }

    /// Committed spend for one budget in one UTC period (`YYYY-MM`), in
    /// micro-USD: `open` and `expired` rows count at their `max_cost` estimate,
    /// `settled` rows at their actual charge.
    ///
    /// Unknown budgets are rejected instead of reporting a misleading `0`.
    pub fn committed_micros(&self, budget: &str, period: &str) -> Result<Micros, LedgerError> {
        validate_period(period)?;
        let conn = self.lock();
        ensure_budget(&conn, budget)?;
        committed_in(&conn, budget, period)
    }

    /// Outstanding reservation rows for a budget: everything not yet settled
    /// (`open` and `expired`), each with its state.
    ///
    /// `expired` rows are included on purpose: they are still unsettled
    /// liabilities that restart/late-charge reconciliation must be able to find
    /// and settle via [`Ledger::settle_by_id`]. Unknown budgets are rejected for
    /// the same reason as in [`Ledger::committed_micros`]. For the full audit
    /// history including settled rows, use [`Ledger::reservations`].
    pub fn open_reservations(&self, budget: &str) -> Result<Vec<ReservationInfo>, LedgerError> {
        let conn = self.lock();
        ensure_budget(&conn, budget)?;
        read_reservation_rows(&conn, budget, true)
    }

    /// All reservation rows for a budget — `open`, `expired` and `settled` —
    /// ordered by `created_at`, then `id`.
    ///
    /// This is the audit view behind the paid gate's disclosure records: every
    /// provider request ever reserved against the budget, with the actual
    /// charge where settlement recorded one. Unknown budgets are rejected for
    /// the same reason as in [`Ledger::committed_micros`].
    pub fn reservations(&self, budget: &str) -> Result<Vec<ReservationInfo>, LedgerError> {
        let conn = self.lock();
        ensure_budget(&conn, budget)?;
        read_reservation_rows(&conn, budget, false)
    }

    /// The connection, tolerating a poisoned mutex: a panic between statements
    /// leaves SQLite consistent (uncommitted work rolls back), so recovering the
    /// guard keeps the daemon alive instead of failing every later write.
    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Marker module sealing [`BudgetLedger`] to in-crate implementations.
pub(crate) mod sealed {
    /// Implemented only by the in-crate `BudgetLedger` implementations; an
    /// outside crate cannot name (or satisfy) this supertrait, so it cannot
    /// implement [`BudgetLedger`](super::BudgetLedger).
    pub trait Sealed {}
}

/// The budget operations the paid gate needs, split out so the gate's
/// fail-closed behavior can be tested against fakes.
///
/// Why this trait is safe: **it is sealed.** The `sealed::Sealed` supertrait is
/// implemented only for types inside this crate, and those are all reviewed
/// here, so no outside implementation can weaken the gate (for example by
/// handing back a ticket minted for a different request). Tickets themselves
/// cannot be minted outside `store::ledger` at all ([`Reservation`]'s fields
/// are private and only [`Ledger::reserve`] constructs one), so even an
/// in-crate fake can only fail or delegate to a real [`Ledger`]. The paid gate
/// dispatches a provider request if and only if this trait hands back a real
/// ticket — and it re-checks that the ticket matches the request first.
///
/// ```compile_fail,E0277
/// use engine::store::ledger::{
///     BudgetLedger, LedgerError, Micros, Reservation, SettleReceipt,
/// };
///
/// struct Outside;
///
/// impl BudgetLedger for Outside {
///     fn reserve(
///         &self,
///         _budget: &str,
///         _provider: &str,
///         _max_cost: Micros,
///         _now_unix: i64,
///     ) -> Result<Reservation, LedgerError> {
///         unimplemented!()
///     }
///
///     fn settle(
///         &self,
///         _reservation: Reservation,
///         _actual: Micros,
///         _now_unix: i64,
///     ) -> Result<SettleReceipt, LedgerError> {
///         unimplemented!()
///     }
/// }
///
/// fn main() {}
/// ```
pub trait BudgetLedger: sealed::Sealed + Send + Sync + 'static {
    /// Reserve the maximum expected cost of one physical attempt; see
    /// [`Ledger::reserve`].
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError>;

    /// Settle an attempt and consume its ticket, reporting the authoritative
    /// charge the ledger recorded; see [`Ledger::settle`].
    fn settle(
        &self,
        reservation: Reservation,
        actual: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError>;
}

impl sealed::Sealed for Ledger {}

impl BudgetLedger for Ledger {
    fn reserve(
        &self,
        budget: &str,
        provider: &str,
        max_cost: Micros,
        now_unix: i64,
    ) -> Result<Reservation, LedgerError> {
        Ledger::reserve(self, budget, provider, max_cost, now_unix)
    }

    fn settle(
        &self,
        reservation: Reservation,
        actual: Micros,
        now_unix: i64,
    ) -> Result<SettleReceipt, LedgerError> {
        Ledger::settle(self, reservation, actual, now_unix)
    }
}

/// Committed spend for `budget` in `period`; see [`Ledger::committed_micros`]
/// for the formula. A settled row with no actual charge falls back to its
/// estimate: an unknown charge must never under-count.
fn committed_in(conn: &Connection, budget: &str, period: &str) -> Result<Micros, LedgerError> {
    let committed: Micros = conn.query_row(
        "SELECT COALESCE(SUM(CASE
             WHEN state = 'settled' THEN COALESCE(actual_micros, max_cost_micros)
             ELSE max_cost_micros
         END), 0)
         FROM reservations
         WHERE budget = ?1 AND period = ?2",
        params![budget, period],
        |row| row.get(0),
    )?;
    Ok(committed)
}

/// Reservation rows for `budget`, ordered by `created_at` then `id`.
///
/// With `only_unsettled`, settled rows are left out (the outstanding-liability
/// view); otherwise every row is returned for auditing.
fn read_reservation_rows(
    conn: &Connection,
    budget: &str,
    only_unsettled: bool,
) -> Result<Vec<ReservationInfo>, LedgerError> {
    let mut stmt = if only_unsettled {
        conn.prepare(
            "SELECT id, provider, max_cost_micros, state, actual_micros, created_at, period
             FROM reservations
             WHERE budget = ?1 AND state != 'settled'
             ORDER BY created_at, id",
        )?
    } else {
        conn.prepare(
            "SELECT id, provider, max_cost_micros, state, actual_micros, created_at, period
             FROM reservations
             WHERE budget = ?1
             ORDER BY created_at, id",
        )?
    };
    let rows = stmt.query_map(params![budget], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Micros>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<Micros>>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, provider, max_cost_micros, state, actual_micros, created_at_unix, period) = row?;
        out.push(ReservationInfo {
            id,
            provider,
            max_cost_micros,
            actual_micros,
            state: parse_state(&state)?,
            created_at_unix,
            period,
        });
    }
    Ok(out)
}

/// Set the WAL journal mode and return the mode SQLite reports.
///
/// Changing the journal mode needs brief exclusive access to the file and
/// fails *immediately* with `SQLITE_BUSY` — the busy handler is not consulted
/// for this pragma — when a concurrent [`Ledger::open`] is mid-migration on the
/// same fresh database. Retrying within the same budget as [`BUSY_TIMEOUT`]
/// keeps concurrent opens reliable; every other error fails closed.
fn set_wal_journal_mode(conn: &mut Connection) -> Result<String, LedgerError> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    loop {
        let attempt = conn.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        });
        match attempt {
            Ok(mode) => return Ok(mode),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == ErrorCode::DatabaseBusy && Instant::now() < deadline =>
            {
                std::thread::sleep(JOURNAL_MODE_RETRY_PAUSE);
            }
            Err(err) => return Err(LedgerError::Storage(err)),
        }
    }
}

fn ensure_budget(conn: &Connection, budget: &str) -> Result<(), LedgerError> {
    let known: Option<String> = conn
        .query_row(
            "SELECT name FROM budgets WHERE name = ?1",
            params![budget],
            |row| row.get(0),
        )
        .optional()?;
    match known {
        Some(_) => Ok(()),
        None => Err(LedgerError::UnknownBudget(budget.to_owned())),
    }
}

/// UTC accounting period (`YYYY-MM`) for a Unix timestamp.
fn period_of(now_unix: i64) -> Result<String, LedgerError> {
    let dt = OffsetDateTime::from_unix_timestamp(now_unix).map_err(|_| {
        LedgerError::InvalidArgument(format!(
            "now_unix {now_unix} is outside the supported time range"
        ))
    })?;
    Ok(format!("{:04}-{:02}", dt.year(), u8::from(dt.month())))
}

fn validate_period(period: &str) -> Result<(), LedgerError> {
    let bytes = period.as_bytes();
    let shape_ok = bytes.len() == 7
        && bytes[4] == b'-'
        && (0..4).all(|i| bytes[i].is_ascii_digit())
        && (5..7).all(|i| bytes[i].is_ascii_digit());
    let month_ok = shape_ok
        && period[5..]
            .parse::<u8>()
            .is_ok_and(|m| (1..=12).contains(&m));
    if month_ok {
        Ok(())
    } else {
        Err(LedgerError::InvalidArgument(format!(
            "period must look like 'YYYY-MM', got {period:?}"
        )))
    }
}

fn parse_state(state: &str) -> Result<ReservationState, LedgerError> {
    match state {
        "open" => Ok(ReservationState::Open),
        "expired" => Ok(ReservationState::Expired),
        "settled" => Ok(ReservationState::Settled),
        other => Err(LedgerError::Corrupt(format!(
            "unexpected reservation state {other:?}"
        ))),
    }
}
