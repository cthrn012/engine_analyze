//! Saves market data as a dated snapshot folder in the format `data::load` reads.
//! The data itself comes from a `Source`, so this file knows nothing about brokers.
//!
//! Each snapshot holds only what was available when it was fetched, which is
//! what keeps later analysis of old snapshots free of hindsight.
use crate::csv::{one_line, quote, Table};
use crate::data::OptionQuote;
use crate::dates::{format_date, parse_ts};
use crate::pricing::Right;
use crate::signals::atm_iv;
use crate::watchlist::HEDGES;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// US markets have closed by this hour UTC all year round.
const CLOSE_HOUR_UTC: i64 = 21;

#[derive(Clone, Debug, PartialEq)]
pub enum SourceError {
    /// The whole fetch should stop, such as a lost connection.
    Fatal(String),
    /// Only this request failed, so the rest of the fetch can carry on.
    Skip(String),
}

impl SourceError {
    fn message(&self) -> &str {
        match self {
            SourceError::Fatal(m) | SourceError::Skip(m) => m,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Story {
    /// `YYYY-MM-DDTHH:MM:SSZ`, the story's first publish time.
    pub available_at: String,
    pub ticker: String,
    pub source: String,
    pub headline: String,
}

/// One contract in an open paper trade.
#[derive(Clone, Debug, PartialEq)]
pub struct HeldLeg {
    pub ticker: String,
    /// Days since 1970-01-01.
    pub expiry_day: i64,
    pub strike: f64,
    pub right: Right,
}

/// Closing prices by ticker, as (date, close) pairs, oldest first.
pub type Closes = BTreeMap<String, Vec<(String, f64)>>;

pub trait Source {
    /// Tickers with no data are left out, not treated as errors.
    fn closes(&self, symbols: &[String]) -> Result<Closes, SourceError>;
    /// `held` must be included even if it falls outside the usual selection.
    fn chain(
        &self,
        ticker: &str,
        spot: f64,
        today: i64,
        held: &[&HeldLeg],
    ) -> Result<Vec<OptionQuote>, SourceError>;
    fn news(&self, tickers: &[String], today: i64) -> Result<Vec<Story>, SourceError>;
}

pub struct Config {
    pub dir: PathBuf,
    /// Tickers to fetch option chains for. Hedge ETFs get prices automatically.
    pub tickers: Vec<String>,
    /// Contracts in open trades, which must stay priced.
    pub held: Vec<HeldLeg>,
    /// Name saved in the snapshot and journal, such as `ibkr`.
    pub feed: String,
}

#[derive(Debug)]
pub struct Report {
    pub date: String,
    pub path: PathBuf,
    pub chains: usize,
    pub quotes: usize,
    pub stories: usize,
    pub warnings: Vec<String>,
}

pub fn stamp(secs: i64) -> String {
    let t = secs.rem_euclid(86400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        format_date(secs.div_euclid(86400)),
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

type IvHistory = BTreeMap<(String, String), f64>;

fn read_iv_history(path: &Path) -> Result<IvHistory, String> {
    let mut out = IvHistory::new();
    if path.exists() {
        for r in Table::read(path)?.iter() {
            out.insert(
                (r.get("ticker")?.to_string(), r.get("date")?.to_string()),
                r.num("atm_iv")?,
            );
        }
    }
    Ok(out)
}

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// The CSV reader works line by line, so a saved field can't contain a line break.
fn news_csv(stories: &[Story], fetched: &str) -> String {
    let mut csv = String::from("available_at,ticker,source,headline,fetched_at\n");
    for n in stories {
        let _ = writeln!(
            csv,
            "{},{},{},{},{fetched}",
            n.available_at,
            n.ticker,
            quote(&one_line(&n.source)),
            quote(&one_line(&n.headline))
        );
    }
    csv
}

/// Downloads everything first and only then writes, so a failed fetch leaves
/// earlier snapshots and the `latest` link untouched.
pub fn run(source: &dyn Source, cfg: &Config, now: i64) -> Result<Report, String> {
    let today = now.div_euclid(86400);
    let mut warnings = Vec::new();

    let mut tickers = cfg.tickers.clone();
    tickers.extend(cfg.held.iter().map(|h| h.ticker.clone()));
    tickers.sort();
    tickers.dedup();

    let mut symbols: Vec<String> = tickers.clone();
    symbols.extend(HEDGES.iter().map(|h| h.to_string()));
    symbols.sort();
    symbols.dedup();

    let closes = source
        .closes(&symbols)
        .map_err(|e| e.message().to_string())?;
    let date = closes
        .values()
        .filter_map(|rows| rows.last())
        .map(|r| r.0.clone())
        .max()
        .ok_or("no prices were returned")?;
    let snapshot_day = parse_ts(&date).ok_or("price dates were not YYYY-MM-DD")? / 86400;
    if snapshot_day == today && now.rem_euclid(86400) / 3600 < CLOSE_HOUR_UTC {
        warnings
            .push("the market may still be open, so today's prices may not be final closes".into());
    }
    for s in &symbols {
        if !closes.contains_key(s) {
            warnings.push(format!("no prices for {s}"));
        }
    }

    let history_path = cfg.dir.join("iv_history.csv");
    let mut history = read_iv_history(&history_path)?;
    let mut options =
        String::from("ticker,expiry_days,strike,right,bid,ask,volume,open_interest\n");
    let (mut chains, mut quotes) = (0, 0);
    for ticker in &tickers {
        let Some(spot) = closes.get(ticker).and_then(|r| r.last()).map(|r| r.1) else {
            continue;
        };
        let held: Vec<&HeldLeg> = cfg.held.iter().filter(|h| h.ticker == *ticker).collect();
        let chain = match source.chain(ticker, spot, snapshot_day, &held) {
            Ok(c) => c,
            Err(SourceError::Fatal(m)) => return Err(m),
            Err(SourceError::Skip(m)) => {
                warnings.push(format!("skipped {ticker}: {m}"));
                continue;
            }
        };
        if chain.is_empty() {
            warnings.push(format!("no usable option quotes for {ticker}"));
            continue;
        }
        for q in &chain {
            let _ = writeln!(
                options,
                "{ticker},{},{},{},{},{},{},{}",
                q.expiry_days,
                q.strike,
                q.right.letter(),
                q.bid,
                q.ask,
                q.volume,
                q.open_interest
            );
        }
        if let Some(iv) = atm_iv(&chain, spot) {
            history.insert((ticker.clone(), date.clone()), iv);
        }
        chains += 1;
        quotes += chain.len();
    }
    if chains == 0 {
        return Err("no option chains could be fetched, so no snapshot was saved".into());
    }

    let stories = match source.news(&tickers, snapshot_day) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("no news saved: {}", e.message()));
            Vec::new()
        }
    };
    let news = news_csv(&stories, &stamp(now));

    let mut prices = String::from("date,ticker,close,atm_iv\n");
    for (ticker, rows) in &closes {
        for (d, close) in rows {
            let iv = history
                .get(&(ticker.clone(), d.clone()))
                .copied()
                .unwrap_or(0.0);
            let _ = writeln!(prices, "{d},{ticker},{close},{iv:.4}");
        }
    }
    let mut iv_csv = String::from("date,ticker,atm_iv\n");
    for ((ticker, d), iv) in &history {
        let _ = writeln!(iv_csv, "{d},{ticker},{iv:.4}");
    }

    fs::create_dir_all(&cfg.dir).map_err(io)?;
    let tmp = cfg.dir.join(".incoming");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(io)?;
    let files = [
        ("prices.csv", prices.as_str()),
        ("options.csv", options.as_str()),
        ("news.csv", news.as_str()),
        ("earnings.csv", "ticker,days_to_earnings\n"),
        ("FEED", cfg.feed.as_str()),
        (
            "EARNINGS_UNKNOWN",
            "This snapshot has no earnings calendar.\n",
        ),
    ];
    for (name, text) in files {
        fs::write(tmp.join(name), text).map_err(io)?;
    }
    let dest = cfg.dir.join(&date);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(io)?;
    }
    fs::rename(&tmp, &dest).map_err(io)?;
    fs::write(&history_path, iv_csv).map_err(io)?;

    let latest = cfg.dir.join("latest");
    let _ = fs::remove_file(&latest);
    std::os::unix::fs::symlink(&date, &latest).map_err(io)?;

    Ok(Report {
        date,
        path: dest,
        chains,
        quotes,
        stories: stories.len(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_readable_timestamps() {
        assert_eq!(stamp(86400 + 3661), "1970-01-02T01:01:01Z");
        assert_eq!(parse_ts(&stamp(1_800_000_000)), Some(1_800_000_000));
    }
}
