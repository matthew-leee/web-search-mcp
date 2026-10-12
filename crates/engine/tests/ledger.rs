//! Contract tests for the durable budget ledger (decisions D6, D13, design :51).
//!
//! All tests are offline: no network, no live or paper API. The concurrency
//! test proves the cap holds across separate connections to one SQLite file,
//! which is the same primitive the engine daemon relies on.

use std::sync::{Arc, Barrier};

use engine::store::ledger::{Ledger, LedgerError, Reservation, ReservationState, SettleReceipt};
use time::{Date, Month, PrimitiveDateTime, Time};

/// Timestamps are built explicitly in UTC so a local-time period derivation
/// (which would put 23:59:59Z in the wrong month here) cannot pass by accident.
fn ts(month: Month, day: u8, hour: u8, minute: u8, second: u8) -> i64 {
    let date = Date::from_calendar_date(2026, month, day).unwrap();
    let time = Time::from_hms(hour, minute, second).unwrap();
    PrimitiveDateTime::new(date, time)
        .assume_utc()
        .unix_timestamp()
}

fn nov(day: u8, hour: u8) -> i64 {
    ts(Month::November, day, hour, 0, 0)
}

#[test]
fn open_rejects_non_durable_memory_paths() {
    // An in-memory ledger loses every liability on restart and gives each
    // connection its own private caps; such paths must be refused before the
    // database is even opened (decision D13).
    for bad in [
        "",
        ":memory:",
        "file::memory:ledger",
        "file:ledger.db?mode=memory",
    ] {
        match Ledger::open(bad) {
            Err(LedgerError::InvalidArgument(msg)) => assert!(!msg.is_empty()),
            other => panic!("path {bad:?} must be rejected as InvalidArgument, got {other:?}"),
        }
    }
}

#[test]
fn reserve_within_cap_is_counted_and_over_cap_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let now = nov(5, 12);
    let ticket = ledger.reserve("main", "exa", 400, now).unwrap();
    assert!(!ticket.id().is_empty());
    assert_eq!(ticket.provider(), "exa");
    assert_eq!(ticket.max_cost_micros(), 400);
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 400);
    let open = ledger.open_reservations("main").unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, ticket.id());
    assert_eq!(open[0].provider, "exa");
    assert_eq!(open[0].max_cost_micros, 400);
    assert_eq!(open[0].state, ReservationState::Open);
    assert_eq!(open[0].period, "2026-11");

    // Over cap: loud error, and nothing at all is written.
    match ledger.reserve("main", "parallel", 601, now).unwrap_err() {
        LedgerError::OverCap {
            budget,
            cap,
            committed,
            requested,
        } => {
            assert_eq!(budget, "main");
            assert_eq!((cap, committed, requested), (1_000, 400, 601));
        }
        other => panic!("expected OverCap, got {other:?}"),
    }
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 400);
    assert_eq!(ledger.open_reservations("main").unwrap().len(), 1);

    // Exactly at the cap is still allowed; one micro-USD more is not.
    let _fill = ledger.reserve("main", "parallel", 600, now).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 1_000);
    match ledger.reserve("main", "exa", 1, now).unwrap_err() {
        LedgerError::OverCap {
            committed,
            requested,
            ..
        } => assert_eq!((committed, requested), (1_000, 1)),
        other => panic!("expected OverCap, got {other:?}"),
    }

    // A zero cap is an explicit kill switch: every dispatch is refused.
    ledger.set_budget("muted", 0).unwrap();
    assert!(matches!(
        ledger.reserve("muted", "exa", 1, now).unwrap_err(),
        LedgerError::OverCap { .. }
    ));
}

#[test]
fn concurrent_reservations_at_the_cap_let_exactly_one_win() {
    // Several iterations, two threads, each with its own Ledger on the same
    // file, each wanting 60% of the cap: the BEGIN IMMEDIATE transaction must
    // let exactly one win on every iteration.
    for iteration in 0..20 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        {
            let ledger = Ledger::open(&path).unwrap();
            ledger.set_budget("main", 1_000_000).unwrap();
        }
        let barrier = Arc::new(Barrier::new(2));
        let now = nov(5, 12);
        let results: Vec<Result<Reservation, LedgerError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = ["exa", "parallel"]
                .into_iter()
                .map(|provider| {
                    let barrier = Arc::clone(&barrier);
                    let path = &path;
                    scope.spawn(move || {
                        let ledger = Ledger::open(path).unwrap();
                        barrier.wait();
                        ledger.reserve("main", provider, 600_000, now)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let wins = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            wins, 1,
            "iteration {iteration}: exactly one of {results:?} may win at the cap"
        );
        for result in &results {
            if let Err(err) = result {
                assert!(
                    matches!(err, LedgerError::OverCap { .. }),
                    "iteration {iteration}: the loser must be OverCap, got {err:?}"
                );
            }
        }
    }
}

#[test]
fn concurrent_first_open_migrates_the_schema_exactly_once() {
    // Four threads race Ledger::open on one fresh path. The schema decision
    // must be made inside the migration's BEGIN IMMEDIATE transaction: two
    // first opens that both see user_version 0 would collide on CREATE TABLE.
    for iteration in 0..10 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let barrier = Arc::new(Barrier::new(4));
        let results: Vec<Result<Ledger, LedgerError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let barrier = Arc::clone(&barrier);
                    let path = &path;
                    scope.spawn(move || {
                        barrier.wait();
                        Ledger::open(path)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (thread, result) in results.iter().enumerate() {
            assert!(
                result.is_ok(),
                "iteration {iteration}, thread {thread}: concurrent open of a fresh ledger must succeed, got {result:?}"
            );
        }
        // The shared schema works: a reservation succeeds afterwards.
        let ledger = Ledger::open(&path).unwrap();
        ledger.set_budget("main", 1_000).unwrap();
        let ticket = ledger.reserve("main", "exa", 400, nov(5, 12)).unwrap();
        assert_eq!(ticket.max_cost_micros(), 400);
        assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 400);
    }
}

#[test]
fn open_liability_survives_restart_and_blocks_over_cap_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let now = nov(5, 12);

    let ticket_id = {
        let ledger = Ledger::open(&path).unwrap();
        ledger.set_budget("main", 1_000).unwrap();
        let ticket = ledger.reserve("main", "exa", 600, now).unwrap();
        let id = ticket.id().to_owned();
        // Crash simulation: ledger and ticket dropped without settling.
        id
    };

    // Reopening an existing database must work and keep the liability counted.
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 600);
    let open = ledger.open_reservations("main").unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, ticket_id);
    match ledger.reserve("main", "parallel", 600, now).unwrap_err() {
        LedgerError::OverCap {
            committed,
            requested,
            ..
        } => assert_eq!((committed, requested), (600, 600)),
        other => panic!("expected OverCap, got {other:?}"),
    }
}

#[test]
fn settle_with_lower_actual_frees_the_difference() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let now = nov(5, 12);
    let ticket = ledger.reserve("main", "exa", 800, now).unwrap();
    let settled = ledger.settle(ticket, 300, now).unwrap();
    // The receipt reports the authoritative recorded charge.
    assert_eq!(
        settled,
        SettleReceipt {
            recorded_micros: 300,
            already_settled: false,
        }
    );
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 300);
    assert!(ledger.open_reservations("main").unwrap().is_empty());

    // The freed 500 is usable again.
    let _second = ledger.reserve("main", "parallel", 700, now).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 1_000);
    assert!(matches!(
        ledger.reserve("main", "exa", 1, now).unwrap_err(),
        LedgerError::OverCap { .. }
    ));
}

#[test]
fn double_settle_by_id_keeps_the_first_actual() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let now = nov(5, 12);
    let ticket = ledger.reserve("main", "exa", 400, now).unwrap();
    let id = ticket.id().to_owned();

    let first = ledger.settle_by_id(&id, 100, now).unwrap();
    assert_eq!(
        first,
        SettleReceipt {
            recorded_micros: 100,
            already_settled: false,
        }
    );
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 100);
    // Idempotent: the replay is Ok, must not count again, and its receipt
    // reports the first amount with `already_settled` set.
    let replay = ledger.settle_by_id(&id, 900, now).unwrap();
    assert_eq!(
        replay,
        SettleReceipt {
            recorded_micros: 100,
            already_settled: true,
        }
    );
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 100);
    assert!(ledger.open_reservations("main").unwrap().is_empty());
}

#[test]
fn settle_after_expiry_records_the_actual() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let t0 = nov(5, 12);
    let ticket = ledger.reserve("main", "exa", 500, t0).unwrap();
    let id = ticket.id().to_owned();
    assert_eq!(ledger.expire_open_older_than(60, t0 + 61).unwrap(), 1);

    // Expired rows stay findable: they are unsettled liabilities.
    let open = ledger.open_reservations("main").unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, ReservationState::Expired);

    // A late charge records the real amount and settles the row.
    ledger.settle_by_id(&id, 120, t0 + 62).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 120);
    assert!(ledger.open_reservations("main").unwrap().is_empty());
}

#[test]
fn reservations_lists_every_row_with_actuals_for_auditing() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let t0 = nov(5, 12);
    // One row in each state: settled (counts at its actual charge), expired
    // (estimate) and open (estimate).
    let settled = ledger.reserve("main", "exa", 300, t0).unwrap();
    let settled_id = settled.id().to_owned();
    ledger.settle(settled, 120, t0).unwrap();
    let expired = ledger.reserve("main", "parallel", 400, t0 + 10).unwrap();
    let expired_id = expired.id().to_owned();
    assert_eq!(ledger.expire_open_older_than(5, t0 + 16).unwrap(), 1);
    let open = ledger.reserve("main", "exa", 200, t0 + 20).unwrap();
    let open_id = open.id().to_owned();

    // The audit view returns ALL rows ordered by created_at, id.
    let rows = ledger.reservations("main").unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        [settled_id.as_str(), expired_id.as_str(), open_id.as_str()]
    );
    assert_eq!(rows[0].state, ReservationState::Settled);
    assert_eq!(rows[0].actual_micros, Some(120));
    assert_eq!(rows[0].provider, "exa");
    assert_eq!(rows[0].period, "2026-11");
    assert_eq!(rows[1].state, ReservationState::Expired);
    assert_eq!(rows[1].actual_micros, None);
    assert_eq!(rows[1].provider, "parallel");
    assert_eq!(rows[2].state, ReservationState::Open);
    assert_eq!(rows[2].actual_micros, None);

    // The outstanding-liability view still hides settled rows.
    let outstanding = ledger.open_reservations("main").unwrap();
    assert_eq!(outstanding.len(), 2);
    assert!(!outstanding.iter().any(|row| row.id == settled_id));

    // Unknown budgets are rejected instead of returning an empty audit log.
    assert!(matches!(
        ledger.reservations("ghost").unwrap_err(),
        LedgerError::UnknownBudget(_)
    ));
}

#[test]
fn reservations_breaks_created_at_ties_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let t0 = nov(5, 12);
    let ids: Vec<String> = (0..4)
        .map(|_| {
            ledger
                .reserve("main", "exa", 100, t0)
                .unwrap()
                .id()
                .to_owned()
        })
        .collect();
    let mut expected = ids;
    expected.sort();

    let rows = ledger.reservations("main").unwrap();
    let got: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
    assert_eq!(got, expected);
}

#[test]
fn expire_open_older_than_charges_the_estimate() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    let t0 = nov(5, 12);
    let _ticket = ledger.reserve("main", "exa", 600, t0).unwrap();
    // Fresh rows are not expired.
    assert_eq!(ledger.expire_open_older_than(100, t0 + 10).unwrap(), 0);
    assert_eq!(ledger.expire_open_older_than(100, t0 + 101).unwrap(), 1);
    // Expiry charges the estimate: the committed spend does not shrink.
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 600);
    // Idempotent: already-expired rows are not touched again.
    assert_eq!(ledger.expire_open_older_than(100, t0 + 200).unwrap(), 0);

    // The estimate keeps blocking until the cap is hit.
    let _second = ledger.reserve("main", "parallel", 400, t0 + 200).unwrap();
    assert!(matches!(
        ledger.reserve("main", "exa", 1, t0 + 200).unwrap_err(),
        LedgerError::OverCap { .. }
    ));
}

#[test]
fn month_boundary_separates_periods() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();

    // Last second of October 2026 UTC vs first second of November 2026 UTC.
    let oct_end = ts(Month::October, 31, 23, 59, 59);
    let nov_start = ts(Month::November, 1, 0, 0, 0);

    let _oct = ledger.reserve("main", "exa", 600, oct_end).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-10").unwrap(), 600);
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 0);

    // October's spend must not eat November's cap room.
    let _nov = ledger.reserve("main", "parallel", 600, nov_start).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 600);
    assert_eq!(ledger.committed_micros("main", "2026-10").unwrap(), 600);

    // But within November the cap still applies.
    match ledger.reserve("main", "exa", 600, nov_start).unwrap_err() {
        LedgerError::OverCap {
            committed,
            requested,
            ..
        } => assert_eq!((committed, requested), (600, 600)),
        other => panic!("expected OverCap, got {other:?}"),
    }
}

#[test]
fn budgets_isolate_liabilities_and_cap_decisions() {
    // Two nonzero budgets (not a zero-cap kill switch): open, settled and
    // expired liabilities in one budget must never move the other budget's
    // committed spend or its cap decisions.
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("operating", 100_000).unwrap();
    ledger.set_budget("eval", 50_000).unwrap();

    let t0 = nov(5, 12);
    // operating carries one liability in each state: settled (counts at its
    // actual charge), expired (counts at its estimate) and open.
    let settled = ledger.reserve("operating", "exa", 20_000, t0).unwrap();
    ledger.settle(settled, 20_000, t0).unwrap();
    let _expired = ledger.reserve("operating", "exa", 40_000, t0).unwrap();
    assert_eq!(ledger.expire_open_older_than(60, t0 + 61).unwrap(), 1);
    ledger
        .reserve("operating", "exa", 30_000, t0 + 100)
        .unwrap();

    // eval's books are untouched by operating's liabilities.
    assert_eq!(
        ledger.committed_micros("operating", "2026-11").unwrap(),
        90_000
    );
    assert_eq!(ledger.committed_micros("eval", "2026-11").unwrap(), 0);

    // eval's cap decisions are its own: it can use its full 50_000 cap even
    // though operating already has 90_000 committed against its 100_000 cap.
    let eval_ticket = ledger
        .reserve("eval", "parallel", 50_000, t0 + 100)
        .unwrap();
    assert_eq!(ledger.committed_micros("eval", "2026-11").unwrap(), 50_000);
    match ledger.reserve("eval", "parallel", 1, t0 + 100).unwrap_err() {
        LedgerError::OverCap {
            budget,
            cap,
            committed,
            requested,
        } => {
            assert_eq!(budget, "eval");
            assert_eq!((cap, committed, requested), (50_000, 50_000, 1));
        }
        other => panic!("expected OverCap, got {other:?}"),
    }

    // And operating's decisions are its own: its liabilities count, eval's do
    // not.
    match ledger
        .reserve("operating", "exa", 10_001, t0 + 100)
        .unwrap_err()
    {
        LedgerError::OverCap { cap, committed, .. } => {
            assert_eq!((cap, committed), (100_000, 90_000))
        }
        other => panic!("expected OverCap, got {other:?}"),
    }
    // Outstanding liabilities are listed per budget as well: operating has its
    // expired estimate and its open reservation, eval only its own ticket.
    let eval_open = ledger.open_reservations("eval").unwrap();
    assert_eq!(eval_open.len(), 1);
    assert_eq!(eval_open[0].id, eval_ticket.id());
    let operating_open = ledger.open_reservations("operating").unwrap();
    let states: Vec<(ReservationState, i64)> = operating_open
        .iter()
        .map(|row| (row.state, row.max_cost_micros))
        .collect();
    assert_eq!(
        states.as_slice(),
        &[
            (ReservationState::Expired, 40_000),
            (ReservationState::Open, 30_000),
        ]
    );

    // operating can spend exactly its remaining 10_000; settled, expired and
    // open liabilities in one budget never move the other's committed spend.
    ledger
        .reserve("operating", "exa", 10_000, t0 + 100)
        .unwrap();
    assert_eq!(
        ledger.committed_micros("operating", "2026-11").unwrap(),
        100_000
    );
    assert_eq!(ledger.committed_micros("eval", "2026-11").unwrap(), 50_000);
}

// Unix-only: the failure is provoked through POSIX file permissions. The
// project builds and tests in WSL2 (Decisions S007).
#[cfg(unix)]
#[test]
fn storage_failure_fails_closed_without_ticket() {
    // File permissions are the only failure seam allowed here (no test-only
    // connection flags, no PRAGMA tricks): the database files are made
    // read-only and the ledger must refuse to hand out a ticket.
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let wal = dir.path().join("ledger.db-wal");
    let shm = dir.path().join("ledger.db-shm");

    // Open while everything is writable: this creates the database, its
    // -wal/-shm companions and the budget. The ledger is kept alive so the
    // companions survive (a clean close deletes them) and the fresh connections
    // below meet read-only files instead of a missing-file state.
    let keeper = Ledger::open(&path).unwrap();
    keeper.set_budget("main", 100_000).unwrap();

    let set_mode = |file: &std::path::Path, mode: u32| {
        if let Ok(metadata) = std::fs::metadata(file) {
            let mut perm = metadata.permissions();
            perm.set_mode(mode);
            std::fs::set_permissions(file, perm).unwrap();
        }
    };

    // Enforcement probe: if this filesystem ignores chmod (e.g. running as
    // root), there is no way to provoke a storage failure via permissions and
    // the test skips with a printed reason instead of passing vacuously.
    let probe = dir.path().join("probe");
    std::fs::write(&probe, b"probe").unwrap();
    set_mode(&probe, 0o444);
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&probe)
        .is_ok()
    {
        eprintln!(
            "SKIP storage_failure_fails_closed_without_ticket: chmod 0444 is not enforced on {:?}; cannot provoke a storage failure via file permissions here",
            dir.path()
        );
        return;
    }

    let now = nov(5, 12);

    // Step 1: the reservation-write failure. With the write-ahead log and the
    // directory read-only (the database file itself must stay writable, or the
    // fresh open fails first) a fresh connection gets as far as the ledger
    // before it cannot write. Which step fails is asserted explicitly, and
    // either way the error must be Storage — no ticket is ever handed out.
    set_mode(&wal, 0o444);
    set_mode(&shm, 0o444);
    set_mode(dir.path(), 0o555);
    match Ledger::open(&path) {
        Ok(ledger) => {
            eprintln!("storage failure surfaced at the reservation write (Ledger::open succeeded)");
            let err = ledger.reserve("main", "exa", 5_000, now).expect_err(
                "reserve must fail closed when the ledger cannot write: no ticket is ever issued",
            );
            assert!(
                matches!(err, LedgerError::Storage(_)),
                "the failed reservation write must surface as Storage, got {err:?}"
            );
        }
        Err(err) => {
            eprintln!(
                "storage failure surfaced at Ledger::open (the write transaction at open is refused)"
            );
            assert!(
                matches!(err, LedgerError::Storage(_)),
                "the open failure must surface as Storage, got {err:?}"
            );
        }
    }

    // Step 2: the open failure. With the database file itself read-only too,
    // the fresh open cannot even reach the reservation path; it must fail as
    // Storage instead of handing out a ticket.
    set_mode(&path, 0o444);
    let err = Ledger::open(&path).expect_err("open of a fully read-only ledger must fail");
    assert!(
        matches!(err, LedgerError::Storage(_)),
        "the open failure must surface as Storage, got {err:?}"
    );

    // Restore permissions so the live connections can close and clean up.
    set_mode(dir.path(), 0o755);
    set_mode(&path, 0o644);
    set_mode(&wal, 0o644);
    set_mode(&shm, 0o644);

    // Nothing was written: no reservation row, no committed spend.
    let ledger = Ledger::open(&path).unwrap();
    assert!(ledger.open_reservations("main").unwrap().is_empty());
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 0);
}

#[test]
fn negative_or_zero_amounts_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    ledger.set_budget("main", 1_000).unwrap();
    let now = nov(5, 12);

    for bad in [0, -5] {
        assert!(matches!(
            ledger.reserve("main", "exa", bad, now).unwrap_err(),
            LedgerError::InvalidAmount(_)
        ));
    }
    assert!(matches!(
        ledger.set_budget("main", -1).unwrap_err(),
        LedgerError::InvalidAmount(_)
    ));
    assert!(matches!(
        ledger.reserve("main", "", 10, now).unwrap_err(),
        LedgerError::InvalidArgument(_)
    ));
    assert!(matches!(
        ledger.set_budget("", 10).unwrap_err(),
        LedgerError::InvalidArgument(_)
    ));

    // A rejected settle consumes the ticket but changes nothing on disk.
    let ticket = ledger.reserve("main", "exa", 400, now).unwrap();
    let id = ticket.id().to_owned();
    assert!(matches!(
        ledger.settle(ticket, -1, now).unwrap_err(),
        LedgerError::InvalidAmount(_)
    ));
    assert!(matches!(
        ledger.settle_by_id(&id, -1, now).unwrap_err(),
        LedgerError::InvalidAmount(_)
    ));
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 400);
    // The row is still reconcilable by id.
    ledger.settle_by_id(&id, 50, now).unwrap();
    assert_eq!(ledger.committed_micros("main", "2026-11").unwrap(), 50);
}

#[test]
fn unknown_budget_and_unknown_reservation_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(dir.path().join("ledger.db")).unwrap();
    let now = nov(5, 12);

    assert!(matches!(
        ledger.reserve("ghost", "exa", 100, now).unwrap_err(),
        LedgerError::UnknownBudget(b) if b == "ghost"
    ));
    assert!(matches!(
        ledger.committed_micros("ghost", "2026-11").unwrap_err(),
        LedgerError::UnknownBudget(_)
    ));
    assert!(matches!(
        ledger.open_reservations("ghost").unwrap_err(),
        LedgerError::UnknownBudget(_)
    ));
    assert!(matches!(
        ledger.settle_by_id("missing", 10, now).unwrap_err(),
        LedgerError::UnknownReservation(_)
    ));

    // Malformed period strings are rejected instead of silently reporting 0.
    ledger.set_budget("main", 1_000).unwrap();
    for bad in ["banana", "2026-13", "20261-1", "2026-1 "] {
        assert!(matches!(
            ledger.committed_micros("main", bad).unwrap_err(),
            LedgerError::InvalidArgument(_)
        ));
    }
}
