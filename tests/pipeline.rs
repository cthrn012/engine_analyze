//! End-to-end checks of the pipeline on generated sample data.
use engine::analyze::{analyze, Context};
use engine::broker::{Broker, Order, PaperBroker};
use engine::data::{load, Market};
use engine::journal::{self, Entry};
use engine::news::KeywordScorer;
use engine::risk::{Book, Decision, Limits};
use engine::sample::generate;
use engine::watchlist::WATCHLIST;
use std::path::PathBuf;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("engine-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn market(name: &str) -> Market {
    let d = temp(name);
    generate(&d, 42, 0).unwrap();
    load(&d).unwrap()
}

fn ctx<'a>(m: &'a Market, l: &'a Limits, b: &'a Book, kill: bool) -> Context<'a> {
    Context {
        market: m,
        scorer: &KeywordScorer,
        theme: None,
        limits: l,
        book: b,
        kill_switch: kill,
    }
}

#[test]
fn sample_data_is_marked_and_reproducible() {
    let (a, b) = (temp("repro-a"), temp("repro-b"));
    generate(&a, 7, 0).unwrap();
    generate(&b, 7, 0).unwrap();
    assert_eq!(
        std::fs::read(a.join("options.csv")).unwrap(),
        std::fs::read(b.join("options.csv")).unwrap()
    );
    assert!(load(&a).unwrap().is_sample);
}

#[test]
fn every_watchlist_name_runs_end_to_end() {
    let (m, l, b) = (market("all"), Limits::default(), Book::default());
    for (t, _, _) in WATCHLIST {
        let r = analyze(&ctx(&m, &l, &b, false), t).unwrap();
        assert!(
            r.signals.atm_iv > 0.05 && r.signals.atm_iv < 1.5,
            "{t}: {}",
            r.signals.atm_iv
        );
        if let Some(c) = &r.best {
            assert!(c.max_loss > 0.0 && c.ev.is_finite());
        }
    }
}

#[test]
fn news_after_the_decision_time_is_excluded() {
    let (m, l, b) = (market("future"), Limits::default(), Book::default());
    let r = analyze(&ctx(&m, &l, &b, false), "INTC").unwrap();
    assert_eq!(r.news.not_yet_available, 1);
    assert_eq!(r.news.duplicates_removed, 1);
    assert!(r
        .news
        .stories
        .iter()
        .all(|s| !s.headline.contains("Future")));
}

#[test]
fn illiquid_chain_produces_no_trade() {
    let (m, l, b) = (market("thin"), Limits::default(), Book::default());
    let r = analyze(&ctx(&m, &l, &b, false), "ENPH").unwrap();
    assert_eq!(r.considered, 0);
    assert!(matches!(r.decision, Decision::Blocked(_)));
}

#[test]
fn kill_switch_blocks_every_name() {
    let (m, l, b) = (market("kill"), Limits::default(), Book::default());
    for (t, _, _) in WATCHLIST {
        let r = analyze(&ctx(&m, &l, &b, true), t).unwrap();
        assert!(!matches!(r.decision, Decision::Approved { .. }), "{t}");
    }
}

#[test]
fn journal_feeds_back_into_risk_limits() {
    let (m, l) = (market("journal"), Limits::default());
    let path = temp("journal-file").join("trades.csv");
    let empty = Book::default();
    let (t, r) = WATCHLIST
        .iter()
        .find_map(|(t, _, _)| {
            let r = analyze(&ctx(&m, &l, &empty, false), t).ok()?;
            matches!(r.decision, Decision::Approved { .. }).then_some((*t, r))
        })
        .expect("sample data should approve at least one trade");
    let Decision::Approved {
        contracts,
        risk,
        hedge,
    } = &r.decision
    else {
        unreachable!()
    };
    let c = r.best.as_ref().unwrap();
    let fill = PaperBroker
        .submit(&Order {
            candidate: c,
            contracts: *contracts,
            hedge: hedge.as_ref(),
        })
        .unwrap();
    let entry = Entry {
        analysis: &r,
        fill: &fill,
        risk: *risk,
        hedge: hedge.as_ref(),
        theme: "blue-wave",
        as_of: "2026-11-05",
        source: "sample",
    };
    journal::append(&path, &entry).unwrap();

    let ledger = journal::load(&path, &m).unwrap();
    let (book, marked) = (ledger.book, ledger.open);
    assert_eq!(book.positions.len(), 1);
    assert!((book.total_risk() - risk).abs() < 0.01);
    // Exiting straight away must cost money: the spread is paid on the way out too.
    let hedge_free = hedge.is_none();
    if hedge_free {
        assert!(marked[0].pnl.unwrap() < 0.0);
    }
    let again = analyze(&ctx(&m, &l, &book, false), t).unwrap();
    assert!(matches!(&again.decision, Decision::Blocked(w) if w.contains("already")));
}

#[test]
fn a_stock_whose_prices_stopped_early_is_not_analyzed() {
    let mut m = market("stale");
    let (t, _, _) = WATCHLIST[0];
    m.prices.get_mut(t).unwrap().pop();
    let (l, b) = (Limits::default(), Book::default());
    let e = analyze(&ctx(&m, &l, &b, false), t).err().unwrap();
    assert!(e.contains("latest price is from"), "{e}");
}
