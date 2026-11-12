//! Volatility and factor signals computed from prices and the option chain.
use crate::data::{Market, OptionQuote, PriceRow};
use crate::pricing::{implied_vol, Right, DIV_YIELD, RATE};
use crate::watchlist::{Member, BENCHMARK};
use std::collections::HashMap;

const YEAR: f64 = 252.0;
pub const MIN_IV_HISTORY: usize = 60;

#[derive(Clone, Debug)]
pub struct Signals {
    pub spot: f64,
    pub realized_vol: f64,
    pub atm_iv: f64,
    /// Where today's IV sits in its 1-year range: 0 = low, 1 = high.
    /// `None` until there are MIN_IV_HISTORY days of IV to compare against.
    pub iv_rank: Option<f64>,
    /// 10% OTM put IV minus 10% OTM call IV.
    pub skew: f64,
    /// ATM straddle price as a fraction of spot.
    pub implied_move: f64,
    pub beta_market: f64,
    pub alpha_annual: f64,
    pub beta_sector: f64,
    /// Last 5 days' return after removing market exposure.
    pub residual_5d: f64,
    pub days_to_earnings: Option<u32>,
    /// True when the data has no earnings calendar, so a date may be missing.
    pub earnings_unknown: bool,
}

pub fn stdev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = xs.iter().sum::<f64>() / xs.len() as f64;
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

/// Ordinary least squares of y on x, returning (intercept, slope).
pub fn ols(x: &[f64], y: &[f64]) -> (f64, f64) {
    let n = x.len().min(y.len());
    if n < 2 {
        return (0.0, 0.0);
    }
    let mx = x[..n].iter().sum::<f64>() / n as f64;
    let my = y[..n].iter().sum::<f64>() / n as f64;
    let sxx: f64 = x[..n].iter().map(|v| (v - mx).powi(2)).sum();
    let sxy: f64 = x[..n]
        .iter()
        .zip(&y[..n])
        .map(|(a, b)| (a - mx) * (b - my))
        .sum();
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    (my - slope * mx, slope)
}

fn log_returns(rows: &[PriceRow]) -> Vec<(String, f64)> {
    rows.windows(2)
        .map(|w| (w[1].date.clone(), (w[1].close / w[0].close).ln()))
        .collect()
}

/// Returns of two series on the dates they share, oldest first, last `n` only.
fn aligned(a: &[PriceRow], b: &[PriceRow], n: usize) -> (Vec<f64>, Vec<f64>) {
    let bmap: HashMap<_, _> = log_returns(b).into_iter().collect();
    let pairs: Vec<(f64, f64)> = log_returns(a)
        .into_iter()
        .filter_map(|(d, r)| bmap.get(&d).map(|x| (r, *x)))
        .collect();
    let start = pairs.len().saturating_sub(n);
    pairs[start..].iter().copied().unzip()
}

fn quote_iv(q: &OptionQuote, spot: f64) -> Option<f64> {
    implied_vol(
        q.right,
        q.mid(),
        spot,
        q.strike,
        q.expiry_days as f64 / 365.0,
        RATE,
        DIV_YIELD,
    )
}

fn nearest(chain: &[OptionQuote], expiry: u32, right: Right, strike: f64) -> Option<&OptionQuote> {
    chain
        .iter()
        .filter(|q| q.expiry_days == expiry && q.right == right)
        .min_by(|a, b| {
            (a.strike - strike)
                .abs()
                .total_cmp(&(b.strike - strike).abs())
        })
}

/// The expiry nearest 30 days with its at-the-money call, put and average IV.
fn atm_quotes(chain: &[OptionQuote], spot: f64) -> Option<(u32, &OptionQuote, &OptionQuote, f64)> {
    let expiry = chain
        .iter()
        .map(|q| q.expiry_days)
        .min_by_key(|d| d.abs_diff(30))?;
    let call = nearest(chain, expiry, Right::Call, spot)?;
    let put = nearest(chain, expiry, Right::Put, spot)?;
    let ivs: Vec<f64> = [call, put]
        .iter()
        .filter_map(|q| quote_iv(q, spot))
        .collect();
    if ivs.is_empty() {
        return None;
    }
    Some((
        expiry,
        call,
        put,
        ivs.iter().sum::<f64>() / ivs.len() as f64,
    ))
}

pub fn atm_iv(chain: &[OptionQuote], spot: f64) -> Option<f64> {
    atm_quotes(chain, spot).map(|a| a.3)
}

pub fn compute(market: &Market, m: &Member) -> Result<Signals, String> {
    let rows = market
        .prices
        .get(&m.ticker)
        .ok_or_else(|| format!("no price history for {}", m.ticker))?;
    let chain = market
        .chains
        .get(&m.ticker)
        .ok_or_else(|| format!("no option chain for {}", m.ticker))?;
    let bench = market
        .prices
        .get(BENCHMARK)
        .ok_or("no benchmark prices (SPY)")?;
    if rows.len() < 61 {
        return Err(format!(
            "{} needs at least 61 days of prices, has {}",
            m.ticker,
            rows.len()
        ));
    }
    let last = &rows[rows.len() - 1];
    // The chain and news are as of the snapshot date, so a stock whose prices
    // stopped earlier would be priced against quotes from a different day.
    if last.date != market.as_of_date {
        return Err(format!(
            "{}'s latest price is from {}, but the data is as of {}",
            m.ticker, last.date, market.as_of_date
        ));
    }
    let spot = last.close;

    let rets: Vec<f64> = log_returns(rows).iter().map(|r| r.1).collect();
    let realized_vol = stdev(&rets[rets.len() - 20..]) * YEAR.sqrt();

    let (expiry, call, put, atm_iv) = atm_quotes(chain, spot)
        .ok_or_else(|| format!("could not solve implied volatility for {}", m.ticker))?;

    let hist: Vec<f64> = rows
        .iter()
        .rev()
        .take(252)
        .map(|r| r.atm_iv)
        .filter(|v| *v > 0.0)
        .collect();
    let (lo, hi) = hist
        .iter()
        .fold((f64::MAX, f64::MIN), |(l, h), v| (l.min(*v), h.max(*v)));
    let iv_rank = (hist.len() >= MIN_IV_HISTORY && hi > lo)
        .then(|| ((atm_iv - lo) / (hi - lo)).clamp(0.0, 1.0));

    let skew = match (
        nearest(chain, expiry, Right::Put, spot * 0.9).and_then(|q| quote_iv(q, spot)),
        nearest(chain, expiry, Right::Call, spot * 1.1).and_then(|q| quote_iv(q, spot)),
    ) {
        (Some(p), Some(c)) => p - c,
        _ => 0.0,
    };

    let (y, x) = aligned(rows, bench, 60);
    let (alpha, beta_market) = ols(&x, &y);
    let tail = y.len().saturating_sub(5);
    let residual_5d = y[tail..]
        .iter()
        .zip(&x[tail..])
        .map(|(a, b)| a - beta_market * b)
        .sum();
    let beta_sector = match market.prices.get(m.hedge) {
        Some(h) => {
            let (y, x) = aligned(rows, h, 60);
            ols(&x, &y).1
        }
        None => beta_market,
    };

    Ok(Signals {
        spot,
        realized_vol,
        atm_iv,
        iv_rank,
        skew,
        implied_move: (call.mid() + put.mid()) / spot,
        beta_market,
        alpha_annual: alpha * YEAR,
        beta_sector,
        residual_5d,
        days_to_earnings: market.earnings_days.get(&m.ticker).copied(),
        earnings_unknown: market.earnings_unknown && !market.earnings_days.contains_key(&m.ticker),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ols_recovers_alpha_and_beta() {
        let x: Vec<f64> = (0..50)
            .map(|i| ((i * 7 % 13) as f64 - 6.0) / 100.0)
            .collect();
        let y: Vec<f64> = x.iter().map(|v| 0.001 + 1.5 * v).collect();
        let (a, b) = ols(&x, &y);
        assert!((a - 0.001).abs() < 1e-12);
        assert!((b - 1.5).abs() < 1e-12);
    }

    #[test]
    fn stdev_is_the_sample_version() {
        assert!(
            (stdev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]) - 2.138089935299395).abs() < 1e-12
        );
    }

    #[test]
    fn alignment_skips_missing_dates() {
        let row = |d: &str, c| PriceRow {
            date: d.into(),
            close: c,
            atm_iv: 0.2,
        };
        let a = [row("1", 100.0), row("2", 110.0), row("3", 121.0)];
        let b = [row("1", 50.0), row("3", 60.0)];
        let (ra, rb) = aligned(&a, &b, 10);
        assert_eq!(ra.len(), 1);
        assert!((ra[0] - 1.1f64.ln()).abs() < 1e-12);
        assert!((rb[0] - 1.2f64.ln()).abs() < 1e-12);
    }
}
