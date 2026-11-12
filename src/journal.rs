//! Append-only record of paper trades and their exits. Trades are numbered by
//! their row in the trades file; exits live in a second file beside it, so the
//! record of what was decided at entry is never rewritten.
use crate::analyze::Analysis;
use crate::broker::{hedge_exit_price, CloseFill, Fill};
use crate::csv::{quote, Row, Table};
use crate::data::{Market, OptionQuote};
use crate::dates::parse_ts;
use crate::exits::Reason;
use crate::fetch::HeldLeg;
use crate::pricing::Right;
use crate::risk::{Book, Hedge, OpenPosition};
use crate::selection::COMMISSION_PER_SHARE;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const HEADER: &str = "as_of,data_source,ticker,group,theme,news_direction,news_confidence,atm_iv,forecast_vol,structure,legs,contracts,fill_per_share,risk,ev_total,hedge_ticker,hedge_shares,hedge_price";
pub const EXIT_HEADER: &str =
    "trade_id,exit_date,reason,days_held,exit_value_per_share,hedge_exit_price,realized_pnl";

pub struct Entry<'a> {
    pub analysis: &'a Analysis,
    pub fill: &'a Fill,
    pub risk: f64,
    pub hedge: Option<&'a Hedge>,
    pub theme: &'a str,
    pub as_of: &'a str,
    pub source: &'a str,
}

fn append_row(path: &Path, header: &str, cells: &[String]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let old = if path.exists() {
        fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        String::new()
    };
    let mut text = String::new();
    // A file saved without a final newline would join the new row onto its last one.
    if !old.is_empty() && !old.ends_with('\n') {
        text.push('\n');
    }
    match old.lines().find(|l| !l.trim().is_empty()) {
        None => {
            text.push_str(header);
            text.push('\n');
        }
        // Rows are read by column name, so appending under another version's
        // header would put values in the wrong columns.
        Some(found) if found.trim_end_matches('\r') != header => {
            return Err(format!(
                "{}: the header doesn't match the columns this version writes.\n  found:    {found}\n  expected: {header}",
                path.display()
            ));
        }
        Some(_) => {}
    }
    text.push_str(&cells.iter().map(|c| quote(c)).collect::<Vec<_>>().join(","));
    text.push('\n');
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

pub fn append(path: &Path, e: &Entry) -> Result<(), String> {
    let a = e.analysis;
    let c = a
        .best
        .as_ref()
        .ok_or("nothing to journal: no trade selected")?;
    let cells = [
        e.as_of.to_string(),
        e.source.to_string(),
        a.member.ticker.clone(),
        a.member.group.to_string(),
        e.theme.to_string(),
        format!("{:.4}", a.news.direction),
        format!("{:.4}", a.news.confidence),
        format!("{:.4}", a.signals.atm_iv),
        format!("{:.4}", a.forecast.vol),
        c.name.to_string(),
        c.legs_code(),
        e.fill.contracts.to_string(),
        format!("{:.4}", e.fill.price),
        format!("{:.2}", e.risk),
        format!("{:.2}", c.ev * 100.0 * e.fill.contracts as f64),
        e.hedge.map_or(String::new(), |h| h.ticker.clone()),
        e.hedge.map_or(String::new(), |h| h.shares.to_string()),
        e.fill
            .hedge_price
            .map_or(String::new(), |p| format!("{p:.4}")),
    ];
    append_row(path, HEADER, &cells)
}

/// Sample trades get their own journal, so invented data can't touch the risk
/// limits or results of real paper trading.
pub fn default_path(is_sample: bool) -> PathBuf {
    let name = if is_sample {
        "sample_trades.csv"
    } else {
        "paper_trades.csv"
    };
    Path::new("journal").join(name)
}

/// The exits file for a trades file: `trades.csv` -> `trades_exits.csv`.
pub fn exits_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("trades");
    path.with_file_name(format!("{stem}_exits.csv"))
}

#[derive(Clone, Debug)]
struct ParsedLeg {
    long: bool,
    right: Right,
    strike: f64,
    /// Days to expiry on the day the trade was opened.
    days: i64,
}

fn parse_legs(code: &str) -> Option<Vec<ParsedLeg>> {
    code.split('|')
        .map(|part| {
            let long = match part.get(..1)? {
                "+" => true,
                "-" => false,
                _ => return None,
            };
            let right = match part.get(1..2)? {
                "C" => Right::Call,
                "P" => Right::Put,
                _ => return None,
            };
            let (strike, days) = part.get(2..)?.split_once('@')?;
            Some(ParsedLeg {
                long,
                right,
                strike: strike.parse().ok()?,
                days: days.parse().ok()?,
            })
        })
        .collect()
}

/// What closing now would bring in per share: sell at the bid, buy at the ask,
/// pay commission. Quotes count days from today, so `elapsed` is taken off.
fn close_value(legs: &[ParsedLeg], chain: &[OptionQuote], elapsed: i64) -> Option<f64> {
    legs.iter().try_fold(0.0, |acc, l| {
        let q = chain.iter().find(|q| {
            q.right == l.right && q.strike == l.strike && q.expiry_days as i64 == l.days - elapsed
        })?;
        Some(acc + if l.long { q.bid } else { -q.ask } - COMMISSION_PER_SHARE)
    })
}

/// Value at expiry, from the stock's last close on or before the expiry date.
fn settle_value(legs: &[ParsedLeg], market: &Market, ticker: &str, expiry_day: i64) -> Option<f64> {
    let spot = market
        .prices
        .get(ticker)?
        .iter()
        .rev()
        .find(|p| parse_ts(&p.date).is_some_and(|t| t / 86400 <= expiry_day))?
        .close;
    Some(
        legs.iter()
            .map(|l| (if l.long { 1.0 } else { -1.0 }) * l.right.intrinsic(spot, l.strike))
            .sum(),
    )
}

#[derive(Clone, Debug)]
pub struct OpenTrade {
    pub id: usize,
    pub ticker: String,
    pub structure: String,
    pub theme: String,
    pub contracts: u32,
    pub fill: f64,
    pub risk: f64,
    pub ev: f64,
    pub days_held: i64,
    pub days_left: i64,
    pub hedge_shares: f64,
    pub hedge_entry: f64,
    /// Exit value per share, or `None` when there is no price for a leg.
    pub exit_value: Option<f64>,
    /// `None` when the trade has no hedge.
    pub hedge_exit: Option<f64>,
    pub pnl: Option<f64>,
}

impl OpenTrade {
    pub fn pnl_at(&self, value: f64, hedge_exit: Option<f64>) -> f64 {
        let hedge = hedge_exit.map_or(0.0, |p| self.hedge_shares * (p - self.hedge_entry));
        (value - self.fill) * 100.0 * self.contracts as f64 + hedge
    }
}

#[derive(Clone, Debug)]
pub struct ClosedTrade {
    pub id: usize,
    pub ticker: String,
    pub structure: String,
    pub theme: String,
    pub reason: String,
    pub exit_date: String,
    pub days_held: i64,
    pub ev: f64,
    pub risk: f64,
    pub pnl: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Ledger {
    pub book: Book,
    pub open: Vec<OpenTrade>,
    pub closed: Vec<ClosedTrade>,
}

struct ExitRow {
    date: String,
    reason: String,
    days_held: i64,
    pnl: f64,
}

fn load_exits(path: &Path) -> Result<HashMap<usize, ExitRow>, String> {
    let mut out = HashMap::new();
    if !path.exists() {
        return Ok(out);
    }
    for r in Table::read(path)?.iter() {
        let row = ExitRow {
            date: r.get("exit_date")?.to_string(),
            reason: r.get("reason")?.to_string(),
            days_held: r.num("days_held")?,
            pnl: r.num("realized_pnl")?,
        };
        out.insert(r.num("trade_id")?, row);
    }
    Ok(out)
}

fn mark(id: usize, r: &Row, market: &Market, today: i64) -> Result<OpenTrade, String> {
    let ticker = r.get("ticker")?.to_string();
    let entry = r.get("as_of")?;
    let entry_day = parse_ts(entry).ok_or_else(|| r.err("bad as_of date"))? / 86400;
    let elapsed = today - entry_day;
    let legs = parse_legs(r.get("legs")?).ok_or_else(|| r.err("could not read legs"))?;
    let days = legs.first().ok_or_else(|| r.err("trade has no legs"))?.days;
    let days_left = days - elapsed;

    let hedge_ticker = r.get("hedge_ticker")?;
    let hedged = !hedge_ticker.is_empty();
    let (hedge_shares, hedge_entry) = if hedged {
        (r.num("hedge_shares")?, r.num("hedge_price")?)
    } else {
        (0.0, 0.0)
    };
    let hedge_exit = market
        .spot(hedge_ticker)
        .map(|p| hedge_exit_price(p, hedge_shares));

    let exit_value = if elapsed < 0 {
        None // The data is older than the trade, so it cannot price it.
    } else if days_left <= 0 {
        settle_value(&legs, market, &ticker, entry_day + days)
    } else {
        market
            .chains
            .get(&ticker)
            .and_then(|c| close_value(&legs, c, elapsed))
    };

    let mut t = OpenTrade {
        id,
        ticker,
        structure: r.get("structure")?.to_string(),
        theme: r.get("theme")?.to_string(),
        contracts: r.num("contracts")?,
        fill: r.num("fill_per_share")?,
        risk: r.num("risk")?,
        ev: r.num("ev_total")?,
        days_held: elapsed,
        days_left,
        hedge_shares,
        hedge_entry,
        exit_value,
        hedge_exit: if hedged { hedge_exit } else { None },
        pnl: None,
    };
    if !hedged || t.hedge_exit.is_some() {
        t.pnl = exit_value.map(|v| t.pnl_at(v, t.hedge_exit));
    }
    Ok(t)
}

/// The exact contracts in open trades, so a fetch always keeps them priced even
/// after the stock has moved away from them.
pub fn held_legs(path: &Path) -> Result<Vec<HeldLeg>, String> {
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let exits = load_exits(&exits_path(path))?;
    for (i, r) in Table::read(path)?.iter().enumerate() {
        if exits.contains_key(&(i + 1)) {
            continue;
        }
        let entry_day = parse_ts(r.get("as_of")?).ok_or_else(|| r.err("bad as_of date"))? / 86400;
        let legs = parse_legs(r.get("legs")?).ok_or_else(|| r.err("could not read legs"))?;
        out.extend(legs.iter().map(|l| HeldLeg {
            ticker: r.get("ticker").unwrap_or_default().to_string(),
            expiry_day: entry_day + l.days,
            strike: l.strike,
            right: l.right,
        }));
    }
    Ok(out)
}

pub fn load(path: &Path, market: &Market) -> Result<Ledger, String> {
    let mut ledger = Ledger::default();
    if !path.exists() {
        return Ok(ledger);
    }
    let today = market.as_of / 86400;
    let exits = load_exits(&exits_path(path))?;
    for (i, r) in Table::read(path)?.iter().enumerate() {
        let id = i + 1;
        if let Some(e) = exits.get(&id) {
            let ticker = r.get("ticker")?.to_string();
            ledger.book.realized_total += e.pnl;
            if e.date == market.as_of_date {
                ledger.book.realized_today += e.pnl;
                ledger.book.closed_today.push(ticker.clone());
            }
            ledger.closed.push(ClosedTrade {
                id,
                ticker,
                structure: r.get("structure")?.to_string(),
                theme: r.get("theme")?.to_string(),
                reason: e.reason.clone(),
                exit_date: e.date.clone(),
                days_held: e.days_held,
                ev: r.num("ev_total")?,
                risk: r.num("risk")?,
                pnl: e.pnl,
            });
            continue;
        }
        let t = mark(id, &r, market, today)?;
        ledger.book.unrealized_pnl += t.pnl.unwrap_or(0.0);
        ledger.book.positions.push(OpenPosition {
            ticker: t.ticker.clone(),
            group: r.get("group")?.to_string(),
            risk: t.risk,
        });
        ledger.open.push(t);
    }
    Ok(ledger)
}

/// Records a close. Returns the realized PnL.
pub fn record_exit(
    path: &Path,
    t: &OpenTrade,
    f: &CloseFill,
    reason: Reason,
    date: &str,
) -> Result<f64, String> {
    let pnl = t.pnl_at(f.value, f.hedge_price);
    let cells = [
        t.id.to_string(),
        date.to_string(),
        reason.label().to_string(),
        t.days_held.to_string(),
        format!("{:.4}", f.value),
        f.hedge_price.map_or(String::new(), |p| format!("{p:.4}")),
        format!("{pnl:.2}"),
    ];
    append_row(&exits_path(path), EXIT_HEADER, &cells)?;
    Ok(pnl)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    pub trades: usize,
    pub wins: usize,
    pub pnl: f64,
    pub forecast_ev: f64,
    pub risk: f64,
}

/// Totals for a set of closed trades. Comparing `pnl` with `forecast_ev` shows
/// whether the model's expected value is showing up in results.
pub fn summarize<'a>(trades: impl Iterator<Item = &'a ClosedTrade>) -> Summary {
    trades.fold(Summary::default(), |mut s, t| {
        s.trades += 1;
        s.wins += (t.pnl > 0.0) as usize;
        s.pnl += t.pnl;
        s.forecast_ev += t.ev;
        s.risk += t.risk;
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("engine-journal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("rows.csv")
    }

    fn row() -> Vec<String> {
        vec!["1".into(), "2".into()]
    }

    #[test]
    fn writes_the_header_once() {
        let p = temp("header-once");
        append_row(&p, "a,b", &row()).unwrap();
        append_row(&p, "a,b", &row()).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a,b\n1,2\n1,2\n");
    }

    #[test]
    fn refuses_a_file_with_another_header() {
        let p = temp("other-header");
        fs::write(&p, "a,c\n1,2\n").unwrap();
        let e = append_row(&p, "a,b", &row()).unwrap_err();
        assert!(e.contains("header doesn't match"), "{e}");
        assert_eq!(fs::read_to_string(&p).unwrap(), "a,c\n1,2\n");
    }

    #[test]
    fn a_missing_final_newline_does_not_join_rows() {
        let p = temp("no-newline");
        fs::write(&p, "a,b\r\n1,2").unwrap();
        append_row(&p, "a,b", &row()).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a,b\r\n1,2\n1,2\n");
    }

    #[test]
    fn an_empty_file_gets_a_header() {
        let p = temp("empty");
        fs::write(&p, "").unwrap();
        append_row(&p, "a,b", &row()).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "a,b\n1,2\n");
    }
}
