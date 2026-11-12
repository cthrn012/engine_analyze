//! Exit behaviour on small hand-built markets, where every number can be
//! worked out by hand.
use engine::broker::{hedge_exit_price, Broker, CloseOrder, PaperBroker};
use engine::data::{load as load_market, Market, OptionQuote, PriceRow};
use engine::dates::parse_ts;
use engine::exits::{evaluate, ExitRules, Reason};
use engine::journal::{self, Ledger, EXIT_HEADER, HEADER};
use engine::pricing::Right;
use engine::sample::generate;
use engine::selection::COMMISSION_PER_SHARE as FEE;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

fn journal_with(rows: &[&str]) -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("engine-exits-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("trades.csv");
    std::fs::write(&path, format!("{HEADER}\n{}\n", rows.join("\n"))).unwrap();
    path
}

/// One long 100 call for $2.00, opened 2026-01-01 with 30 days to expiry.
const LONG_CALL: &str =
    "2026-01-01,sample,TST,chips,blue-wave,0,0,0.3,0.3,long call,+C100@30,1,2.0000,200.00,15.00,,,";

fn market(date: &str, closes: &[(&str, f64)], quotes: Vec<OptionQuote>) -> Market {
    let mut m = Market {
        as_of_date: date.into(),
        as_of: parse_ts(date).unwrap() + 21 * 3600,
        ..Market::default()
    };
    m.prices.insert(
        "TST".into(),
        closes
            .iter()
            .map(|(d, c)| PriceRow {
                date: d.to_string(),
                close: *c,
                atm_iv: 0.3,
            })
            .collect(),
    );
    m.chains.insert("TST".into(), quotes);
    m
}

fn quote(right: Right, strike: f64, days: u32, bid: f64, ask: f64) -> OptionQuote {
    OptionQuote {
        expiry_days: days,
        strike,
        right,
        bid,
        ask,
        volume: 500,
        open_interest: 1000,
    }
}

fn close_all(path: &Path, m: &Market, l: &Ledger) -> usize {
    let mut n = 0;
    for t in &l.open {
        if let Some(r) = evaluate(&ExitRules::default(), t.risk, t.pnl, t.days_left) {
            let f = PaperBroker
                .close(&CloseOrder {
                    value: t.exit_value.unwrap(),
                    hedge_price: t.hedge_exit,
                })
                .unwrap();
            journal::record_exit(path, t, &f, r, &m.as_of_date).unwrap();
            n += 1;
        }
    }
    n
}

#[test]
fn quotes_are_matched_by_days_remaining_not_days_at_entry() {
    let path = journal_with(&[LONG_CALL]);
    // Ten days on, the 30-day option is the 20-day quote. The 30-day quote here is a different contract.
    let m = market(
        "2026-01-11",
        &[("2026-01-11", 100.0)],
        vec![
            quote(Right::Call, 100.0, 30, 9.0, 9.1),
            quote(Right::Call, 100.0, 20, 2.5, 2.6),
        ],
    );
    let t = &journal::load(&path, &m).unwrap().open[0];
    assert_eq!((t.days_held, t.days_left), (10, 20));
    assert!((t.pnl.unwrap() - (2.5 - FEE - 2.0) * 100.0).abs() < 1e-9);
}

#[test]
fn profit_and_stop_exits_use_the_sell_price_and_pay_commission() {
    for (bid, want) in [(3.5, Reason::TakeProfit), (0.9, Reason::StopLoss)] {
        let path = journal_with(&[LONG_CALL]);
        let m = market(
            "2026-01-11",
            &[("2026-01-11", 100.0)],
            vec![quote(Right::Call, 100.0, 20, bid, bid + 0.1)],
        );
        let l = journal::load(&path, &m).unwrap();
        let t = &l.open[0];
        assert_eq!(
            evaluate(&ExitRules::default(), t.risk, t.pnl, t.days_left),
            Some(want)
        );
        assert_eq!(close_all(&path, &m, &l), 1);

        let after = journal::load(&path, &m).unwrap();
        assert!(after.open.is_empty() && after.book.positions.is_empty());
        assert_eq!(after.closed[0].reason, want.label());
        assert!((after.book.realized_today - (bid - FEE - 2.0) * 100.0).abs() < 0.01);
    }
}

#[test]
fn a_flat_position_closes_on_time() {
    let path = journal_with(&[LONG_CALL]);
    let m = market(
        "2026-01-25",
        &[("2026-01-25", 100.0)],
        vec![quote(Right::Call, 100.0, 6, 2.0, 2.1)],
    );
    let t = &journal::load(&path, &m).unwrap().open[0];
    assert_eq!(
        evaluate(&ExitRules::default(), t.risk, t.pnl, t.days_left),
        Some(Reason::TimeExit)
    );
}

#[test]
fn expiry_settles_at_the_close_on_expiry_day_not_the_latest_price() {
    let path = journal_with(&[LONG_CALL]);
    let m = market(
        "2026-02-05",
        &[
            ("2026-01-30", 104.0),
            ("2026-01-31", 105.0),
            ("2026-02-05", 140.0),
        ],
        vec![],
    );
    let t = &journal::load(&path, &m).unwrap().open[0];
    assert_eq!(t.days_left, -5);
    assert_eq!(t.exit_value, Some(5.0));
    assert!((t.pnl.unwrap() - 300.0).abs() < 1e-9);
    assert_eq!(
        evaluate(&ExitRules::default(), t.risk, t.pnl, t.days_left),
        Some(Reason::Expired)
    );
}

#[test]
fn credit_spreads_pay_the_ask_to_close_the_short_leg() {
    // Sold the 95 put and bought the 90 put for a $1.50 credit; $3.50 at risk.
    let row = "2026-01-01,sample,TST,chips,,0,0,0.3,0.3,bull put credit spread,-P95@30|+P90@30,1,-1.5000,350.00,10.00,,,";
    let path = journal_with(&[row]);
    let m = market(
        "2026-01-11",
        &[("2026-01-11", 100.0)],
        vec![
            quote(Right::Put, 95.0, 20, 0.7, 0.8),
            quote(Right::Put, 90.0, 20, 0.2, 0.3),
        ],
    );
    let t = &journal::load(&path, &m).unwrap().open[0];
    assert!((t.exit_value.unwrap() - (0.2 - 0.8 - 2.0 * FEE)).abs() < 1e-12);
    assert!((t.pnl.unwrap() - (1.5 - 0.6 - 2.0 * FEE) * 100.0).abs() < 1e-9);
}

#[test]
fn hedge_pnl_is_part_of_the_result() {
    let row = "2026-01-01,sample,TST,chips,,0,0,0.3,0.3,long call,+C100@30,1,2.0000,200.00,15.00,HDG,-10,50.0000";
    let path = journal_with(&[row]);
    let mut m = market(
        "2026-01-11",
        &[("2026-01-11", 100.0)],
        vec![quote(Right::Call, 100.0, 20, 2.5, 2.6)],
    );
    m.prices.insert(
        "HDG".into(),
        vec![PriceRow {
            date: "2026-01-11".into(),
            close: 48.0,
            atm_iv: 0.2,
        }],
    );
    let t = &journal::load(&path, &m).unwrap().open[0];
    let hedge = -10.0 * (hedge_exit_price(48.0, -10.0) - 50.0);
    assert!(
        hedge > 19.0 && hedge < 20.0,
        "buying back costs a little more than the close"
    );
    assert!((t.pnl.unwrap() - ((2.5 - FEE - 2.0) * 100.0 + hedge)).abs() < 1e-9);
}

#[test]
fn a_position_with_no_price_is_held_but_keeps_its_risk() {
    let path = journal_with(&[LONG_CALL]);
    let m = market("2026-01-25", &[("2026-01-25", 100.0)], vec![]);
    let l = journal::load(&path, &m).unwrap();
    assert_eq!(l.open[0].pnl, None);
    assert_eq!(
        evaluate(
            &ExitRules::default(),
            200.0,
            l.open[0].pnl,
            l.open[0].days_left
        ),
        None
    );
    assert_eq!(l.book.total_risk(), 200.0);
}

#[test]
fn data_older_than_the_trade_cannot_close_it() {
    let path = journal_with(&[LONG_CALL]);
    let m = market(
        "2025-12-20",
        &[("2025-12-20", 100.0)],
        vec![quote(Right::Call, 100.0, 42, 9.0, 9.1)],
    );
    assert_eq!(journal::load(&path, &m).unwrap().open[0].pnl, None);
}

#[test]
fn closing_twice_is_impossible_and_later_days_keep_the_result() {
    let path = journal_with(&[LONG_CALL, LONG_CALL]);
    std::fs::write(
        journal::exits_path(&path),
        format!("{EXIT_HEADER}\n1,2026-01-11,stop loss,10,0.9000,,-110.00\n"),
    )
    .unwrap();
    let m = market(
        "2026-01-12",
        &[("2026-01-12", 100.0)],
        vec![quote(Right::Call, 100.0, 19, 2.0, 2.1)],
    );
    let l = journal::load(&path, &m).unwrap();
    assert_eq!(l.open.iter().map(|t| t.id).collect::<Vec<_>>(), vec![2]);
    assert_eq!(l.book.realized_total, -110.0);
    assert_eq!(
        l.book.realized_today, 0.0,
        "yesterday's loss is not today's"
    );
    assert!(l.book.closed_today.is_empty());
}

#[test]
fn a_trade_cannot_have_two_exit_rows() {
    let path = journal_with(&[LONG_CALL]);
    std::fs::write(
        journal::exits_path(&path),
        format!(
            "{EXIT_HEADER}\n1,2026-01-11,stop loss,10,0.9000,,-110.00\n1,2026-01-12,take profit,11,3.0000,,100.00\n"
        ),
    )
    .unwrap();
    let m = market("2026-01-12", &[("2026-01-12", 100.0)], vec![]);
    let e = journal::load(&path, &m).err().unwrap();
    assert!(e.contains("trade 1 already has an exit"), "{e}");
}

#[test]
fn an_exit_for_a_trade_that_does_not_exist_is_refused() {
    let path = journal_with(&[LONG_CALL]);
    std::fs::write(
        journal::exits_path(&path),
        format!("{EXIT_HEADER}\n2,2026-01-11,stop loss,10,0.9000,,-110.00\n"),
    )
    .unwrap();
    let m = market("2026-01-12", &[("2026-01-12", 100.0)], vec![]);
    let e = journal::load(&path, &m).err().unwrap();
    assert!(e.contains("trade 2, which is not in"), "{e}");
}

#[test]
fn summary_compares_results_with_forecasts() {
    let path = journal_with(&[LONG_CALL, LONG_CALL]);
    std::fs::write(
        journal::exits_path(&path),
        format!("{EXIT_HEADER}\n1,2026-01-11,take profit,10,3.0000,,100.00\n2,2026-01-11,stop loss,10,1.0000,,-120.00\n"),
    )
    .unwrap();
    let m = market("2026-01-11", &[("2026-01-11", 100.0)], vec![]);
    let s = journal::summarize(journal::load(&path, &m).unwrap().closed.iter());
    assert_eq!((s.trades, s.wins), (2, 1));
    assert_eq!(s.pnl, -20.0);
    assert_eq!(s.forecast_ev, 30.0);
}

#[test]
fn sample_snapshots_share_one_history_and_keep_contracts_listed() {
    let base = std::env::temp_dir().join(format!("engine-snap-{}", std::process::id()));
    let (a, b) = (base.join("a"), base.join("b"));
    generate(&a, 42, 0).unwrap();
    generate(&b, 42, 10).unwrap();
    let (m0, m1) = (load_market(&a).unwrap(), load_market(&b).unwrap());
    assert!(m1.as_of > m0.as_of);

    let (old, new) = (&m0.prices["INTC"], &m1.prices["INTC"]);
    assert_eq!(new.len(), old.len() + 10);
    assert!(old
        .iter()
        .zip(new)
        .all(|(x, y)| x.date == y.date && x.close == y.close));

    let elapsed = ((m1.as_of - m0.as_of) / 86400) as u32;
    let later: Vec<u32> = m1.chains["INTC"].iter().map(|q| q.expiry_days).collect();
    assert!(m0.chains["INTC"]
        .iter()
        .filter(|q| q.expiry_days > elapsed)
        .all(|q| later.contains(&(q.expiry_days - elapsed))));
    assert!(generate(&a, 42, 41).is_err());
}

#[test]
fn sample_and_real_trades_default_to_separate_journals() {
    assert_ne!(journal::default_path(true), journal::default_path(false));
    assert_eq!(
        journal::default_path(false),
        Path::new("journal/paper_trades.csv")
    );
}
