//! The risk manager sits between trade selection and the broker. Every limit
//! is checked here, and a blocked decision cannot be turned into an order.
use crate::selection::Candidate;

#[derive(Clone, Debug)]
pub struct Limits {
    pub equity: f64,
    pub max_trade_risk: f64,
    pub max_group_risk: f64,
    pub max_total_risk: f64,
    pub max_loss: f64,
    pub hedge_threshold: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            equity: 100_000.0,
            max_trade_risk: 0.01,
            max_group_risk: 0.04,
            max_total_risk: 0.10,
            max_loss: 0.02,
            hedge_threshold: 5_000.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpenPosition {
    pub ticker: String,
    pub group: String,
    pub risk: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Book {
    pub positions: Vec<OpenPosition>,
    pub unrealized_pnl: f64,
    pub realized_today: f64,
    pub realized_total: f64,
    /// Tickers exited today, which can't be re-entered until the next session.
    pub closed_today: Vec<String>,
}

impl Book {
    pub fn total_risk(&self) -> f64 {
        self.positions.iter().map(|p| p.risk).sum()
    }

    pub fn group_risk(&self, group: &str) -> f64 {
        self.positions
            .iter()
            .filter(|p| p.group == group)
            .map(|p| p.risk)
            .sum()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hedge {
    pub ticker: String,
    /// Negative = sell short.
    pub shares: i64,
    pub price: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Approved {
        contracts: u32,
        risk: f64,
        hedge: Option<Hedge>,
    },
    Blocked(String),
}

pub struct Request<'a> {
    pub ticker: &'a str,
    pub group: &'a str,
    pub candidate: &'a Candidate,
    pub spot: f64,
    pub beta_sector: f64,
    pub hedge_ticker: &'a str,
    pub hedge_price: Option<f64>,
}

pub fn decide(l: &Limits, kill_switch: bool, book: &Book, r: &Request) -> Decision {
    if kill_switch {
        return Decision::Blocked("kill switch is on".into());
    }
    // Realized losses count too, or closing a loser would reset the limit.
    let pnl_today = book.unrealized_pnl + book.realized_today;
    if pnl_today <= -l.max_loss * l.equity {
        return Decision::Blocked(format!(
            "loss limit hit: open and today's closed PnL is ${pnl_today:.0}"
        ));
    }
    if book.positions.iter().any(|p| p.ticker == r.ticker) {
        return Decision::Blocked(format!("already holding a position in {}", r.ticker));
    }

    if book.closed_today.iter().any(|t| t == r.ticker) {
        return Decision::Blocked(format!("{} was already closed today", r.ticker));
    }

    // Selection only builds trades with a positive, finite max loss, but this is
    // the last check before the broker, so it doesn't rely on that: a zero or
    // NaN here would size the trade to an unlimited number of contracts.
    let per_contract = r.candidate.max_loss * 100.0;
    if !(per_contract.is_finite() && per_contract > 0.0) {
        return Decision::Blocked(format!(
            "max loss of ${:.2}/sh can't be used to size the trade",
            r.candidate.max_loss
        ));
    }
    if !(r.spot.is_finite() && r.spot > 0.0 && r.candidate.delta.is_finite()) {
        return Decision::Blocked("spot or delta is not a usable number".into());
    }
    let budget = (l.max_trade_risk * l.equity)
        .min(l.max_group_risk * l.equity - book.group_risk(r.group))
        .min(l.max_total_risk * l.equity - book.total_risk());
    let contracts = (budget / per_contract).floor();
    if contracts < 1.0 {
        return Decision::Blocked(format!(
            "one contract risks ${per_contract:.0}, but only ${:.0} of the risk budget is left",
            budget.max(0.0)
        ));
    }

    let delta_dollars = r.candidate.delta * 100.0 * contracts * r.spot;
    let hedge = match r.hedge_price {
        Some(price) if delta_dollars.abs() >= l.hedge_threshold => {
            let shares = (-delta_dollars * r.beta_sector / price).round() as i64;
            (shares != 0).then(|| Hedge {
                ticker: r.hedge_ticker.to_string(),
                shares,
                price,
            })
        }
        _ => None,
    };
    Decision::Approved {
        contracts: contracts as u32,
        risk: contracts * per_contract,
        hedge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(max_loss: f64, delta: f64) -> Candidate {
        Candidate {
            name: "t",
            legs: vec![],
            expiry_days: 30,
            entry: max_loss,
            max_loss,
            ev: 1.0,
            prob_profit: 0.5,
            delta,
        }
    }

    fn req<'a>(c: &'a Candidate) -> Request<'a> {
        Request {
            ticker: "INTC",
            group: "chips",
            candidate: c,
            spot: 100.0,
            beta_sector: 1.0,
            hedge_ticker: "SMH",
            hedge_price: Some(50.0),
        }
    }

    fn pos(t: &str, g: &str, risk: f64) -> OpenPosition {
        OpenPosition {
            ticker: t.into(),
            group: g.into(),
            risk,
        }
    }

    fn blocked(d: Decision) -> String {
        match d {
            Decision::Blocked(s) => s,
            other => panic!("expected a block, got {other:?}"),
        }
    }

    #[test]
    fn sizes_to_the_per_trade_budget() {
        let c = cand(3.0, 0.0);
        match decide(&Limits::default(), false, &Book::default(), &req(&c)) {
            Decision::Approved {
                contracts,
                risk,
                hedge,
            } => {
                assert_eq!(contracts, 3);
                assert_eq!(risk, 900.0);
                assert_eq!(hedge, None);
            }
            d => panic!("{d:?}"),
        }
    }

    #[test]
    fn a_max_loss_that_cannot_size_the_trade_is_blocked() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let c = cand(bad, 0.0);
            assert!(
                blocked(decide(
                    &Limits::default(),
                    false,
                    &Book::default(),
                    &req(&c)
                ))
                .contains("can't be used to size"),
                "{bad}"
            );
        }
        let c = cand(1.0, f64::NAN);
        assert!(blocked(decide(
            &Limits::default(),
            false,
            &Book::default(),
            &req(&c)
        ))
        .contains("not a usable number"));
    }

    #[test]
    fn kill_switch_beats_everything() {
        let c = cand(1.0, 0.0);
        assert!(
            blocked(decide(&Limits::default(), true, &Book::default(), &req(&c))).contains("kill")
        );
    }

    #[test]
    fn loss_limit_blocks_new_trades() {
        let c = cand(1.0, 0.0);
        let book = Book {
            positions: vec![],
            unrealized_pnl: -2_000.0,
            ..Book::default()
        };
        assert!(blocked(decide(&Limits::default(), false, &book, &req(&c))).contains("loss limit"));
    }

    #[test]
    fn closed_losses_still_count_towards_the_loss_limit() {
        let c = cand(1.0, 0.0);
        let book = Book {
            realized_today: -1_500.0,
            unrealized_pnl: -500.0,
            ..Book::default()
        };
        assert!(blocked(decide(&Limits::default(), false, &book, &req(&c))).contains("loss limit"));
        let yesterday = Book {
            realized_total: -5_000.0,
            ..Book::default()
        };
        assert!(matches!(
            decide(&Limits::default(), false, &yesterday, &req(&c)),
            Decision::Approved { .. }
        ));
    }

    #[test]
    fn no_same_day_reentry_after_an_exit() {
        let c = cand(1.0, 0.0);
        let book = Book {
            closed_today: vec!["INTC".into()],
            ..Book::default()
        };
        assert!(
            blocked(decide(&Limits::default(), false, &book, &req(&c))).contains("closed today")
        );
    }

    #[test]
    fn refuses_a_second_position_in_one_ticker() {
        let c = cand(1.0, 0.0);
        let book = Book {
            positions: vec![pos("INTC", "chips", 100.0)],
            unrealized_pnl: 0.0,
            ..Book::default()
        };
        assert!(blocked(decide(&Limits::default(), false, &book, &req(&c))).contains("already"));
    }

    #[test]
    fn group_and_total_limits_shrink_or_block_size() {
        let c = cand(1.0, 0.0);
        let book = Book {
            positions: vec![pos("AMD", "chips", 3_800.0)],
            unrealized_pnl: 0.0,
            ..Book::default()
        };
        match decide(&Limits::default(), false, &book, &req(&c)) {
            Decision::Approved { contracts, .. } => assert_eq!(contracts, 2),
            d => panic!("{d:?}"),
        }
        let full = Book {
            positions: vec![pos("XOM", "oil", 10_000.0)],
            unrealized_pnl: 0.0,
            ..Book::default()
        };
        assert!(blocked(decide(&Limits::default(), false, &full, &req(&c))).contains("budget"));
    }

    #[test]
    fn hedge_offsets_delta_using_sector_beta() {
        let c = cand(1.0, 0.6);
        let mut r = req(&c);
        r.beta_sector = 1.5;
        match decide(&Limits::default(), false, &Book::default(), &r) {
            // 10 contracts x 100 x 0.6 x $100 = $60,000 of delta.
            Decision::Approved {
                contracts: 10,
                hedge: Some(h),
                ..
            } => {
                assert_eq!(h.ticker, "SMH");
                assert_eq!(h.shares, -1800);
            }
            d => panic!("{d:?}"),
        }
    }
}
