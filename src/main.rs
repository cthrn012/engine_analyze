use engine::analyze::{analyze, Analysis, Context};
use engine::broker::{Broker, CloseOrder, Order, PaperBroker};
use engine::data::{self, Market};
use engine::exits::{self, ExitRules};
use engine::fetch;
use engine::ibkr::{Ibkr, Mode};
use engine::journal::{self, ClosedTrade, Entry, Ledger};
use engine::news::KeywordScorer;
use engine::news_check::{self, Label, LABELS};
use engine::orders;
use engine::risk::{Decision, Limits};
use engine::sample;
use engine::watchlist::{self, Theme, WATCHLIST};
use std::io::{stdin, stdout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "\
Paper-only options engine

USAGE:
  engine help
  engine analyze <TICKER> [--paper] [--theme NAME] [--data DIR] [--journal FILE]
  engine scan [--theme NAME] [--data DIR] [--journal FILE]
  engine exits [--paper] [--data DIR] [--journal FILE]
  engine positions [--data DIR] [--journal FILE]
  engine fetch [TICKERS...] [--delayed] [--port N] [--data DIR] [--journal FILE]
  engine news-grade [--source FILE] [--labels FILE]
  engine news-check [--labels FILE]
  engine orders [--orders FILE]
  engine gen-sample [--data DIR] [--seed N] [--advance DAYS]

  --paper   record the approved trade or the exits in the paper journal
  --advance write a snapshot this many trading days later, to simulate time
  --delayed use IBKR's free delayed data instead of a paid subscription
  --port    TWS or IB Gateway paper port; 7497 and 4002 are tried by default
  --data    data folder, e.g. data/live/latest after running fetch
  --source  CSV with a headline column; defaults to the practice headlines
  --labels  where your grades are kept
  --theme   tag news and journal rows with a theme, e.g. blue-wave

Create a file named KILL_SWITCH in this folder to block all new trades.
Exits still work with it on, because closing a position reduces risk.";

#[derive(Default)]
struct Args {
    positional: Vec<String>,
    paper: bool,
    theme: Option<String>,
    data: Option<PathBuf>,
    journal: Option<PathBuf>,
    seed: Option<u64>,
    advance: usize,
    delayed: bool,
    orders: Option<PathBuf>,
    source: Option<PathBuf>,
    labels: Option<PathBuf>,
    port: Option<u16>,
    help: bool,
}

fn parse(raw: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = raw.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--help" | "-h" => a.help = true,
            "--paper" => a.paper = true,
            "--delayed" => a.delayed = true,
            "--orders" => a.orders = Some(value()?.into()),
            "--source" => a.source = Some(value()?.into()),
            "--labels" => a.labels = Some(value()?.into()),
            "--port" => a.port = Some(value()?.parse().map_err(|_| "--port needs a port number")?),
            "--theme" => a.theme = Some(value()?),
            "--data" => a.data = Some(value()?.into()),
            "--journal" => a.journal = Some(value()?.into()),
            "--advance" => {
                a.advance = value()?
                    .parse()
                    .map_err(|_| "--advance needs a whole number")?
            }
            "--seed" => {
                a.seed = Some(
                    value()?
                        .parse()
                        .map_err(|_| "--seed needs a whole number")?,
                )
            }
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            s => a.positional.push(s.to_string()),
        }
    }
    Ok(a)
}

fn sample_banner(m: &Market) {
    if m.is_sample {
        println!("*** SAMPLE DATA: invented prices and news, not real market information ***\n");
    } else if m.feed.as_deref() == Some(Mode::Delayed.feed()) {
        println!(
            "*** DELAYED DATA: quotes are 15+ minutes old, so edge and fills are unreliable ***\n"
        );
    }
}

fn rank(s: &engine::signals::Signals) -> String {
    s.iv_rank
        .map_or("n/a".to_string(), |r| format!("{:.0}%", r * 100.0))
}

fn print_analysis(a: &Analysis, m: &Market, elapsed_ms: f64) {
    let s = &a.signals;
    println!(
        "{} ({}) as of {}   spot {:.2}",
        a.member.ticker, a.member.group, m.as_of_date, s.spot
    );
    println!("\nVolatility");
    println!(
        "  ATM IV {:.1}%   realized 20d {:.1}%   IV rank {}",
        s.atm_iv * 100.0,
        s.realized_vol * 100.0,
        rank(s)
    );
    println!(
        "  skew {:+.1} pts   implied move {:.1}%",
        s.skew * 100.0,
        s.implied_move * 100.0
    );
    match s.days_to_earnings {
        Some(d) => println!("  earnings in {d} days"),
        None if s.earnings_unknown => {
            println!("  earnings date unknown, so no volatility edge is assumed")
        }
        None => println!("  no earnings date on file"),
    }
    println!("\nFactors (60 days)");
    println!(
        "  beta vs SPY {:.2}   beta vs {} {:.2}",
        s.beta_market, a.member.hedge, s.beta_sector
    );
    println!(
        "  alpha {:+.1}%/yr   5-day residual {:+.1}%",
        s.alpha_annual * 100.0,
        s.residual_5d * 100.0
    );

    let n = &a.news;
    println!("\nNews");
    println!(
        "  direction {:+.2}   confidence {:.2}   {} stories, {} repeats removed, {} not yet available",
        n.direction, n.confidence, n.stories.len(), n.duplicates_removed, n.not_yet_available
    );
    for st in &n.stories {
        let tag = if st.theme_match { " [theme]" } else { "" };
        println!(
            "  {:+.2}  {:>4.0}h ago  {}{tag}",
            st.score, st.age_hours, st.headline
        );
    }

    println!("\nForecast");
    println!(
        "  expected move {:+.2}%   volatility {:.1}%",
        a.forecast.expected_move * 100.0,
        a.forecast.vol * 100.0
    );

    println!("\nTrade ({} structures considered)", a.considered);
    if let Some(c) = &a.best {
        println!("  {}, {} days: {}", c.name, c.expiry_days, c.legs_code());
        let kind = if c.entry >= 0.0 { "debit" } else { "credit" };
        println!(
            "  {kind} ${:.2}/sh   max loss ${:.2}/sh   EV ${:+.2}/sh",
            c.entry.abs(),
            c.max_loss,
            c.ev
        );
        println!(
            "  edge {:.1}% of risk   model P(profit) {:.0}%   delta {:+.2}",
            c.edge() * 100.0,
            c.prob_profit * 100.0,
            c.delta
        );
    }
    match &a.decision {
        Decision::Approved {
            contracts,
            risk,
            hedge,
        } => {
            println!("\nRisk check: APPROVED  {contracts} contracts, ${risk:.0} at risk");
            if let Some(h) = hedge {
                println!(
                    "  hedge: {} {} {} at about {:.2}",
                    if h.shares < 0 { "short" } else { "buy" },
                    h.shares.abs(),
                    h.ticker,
                    h.price
                );
            }
        }
        Decision::Blocked(why) => println!("\nRisk check: NO TRADE  {why}"),
    }
    println!("\nAnalysis took {elapsed_ms:.2} ms");
}

struct Session {
    market: Market,
    ledger: Ledger,
    limits: Limits,
    theme: Option<Theme>,
    kill: bool,
    journal: PathBuf,
}

fn open(a: &Args) -> Result<Session, String> {
    let dir = a.data.clone().unwrap_or_else(|| "data/sample".into());
    let market = data::load(&dir).map_err(|e| {
        if dir.join("prices.csv").exists() {
            e
        } else {
            format!("{e}\nRun `engine gen-sample` first.")
        }
    })?;
    let journal = a
        .journal
        .clone()
        .unwrap_or_else(|| journal::default_path(market.is_sample));
    let ledger = journal::load(&journal, &market)?;
    let mut limits = Limits::default();
    limits.equity += ledger.book.realized_total;
    let theme = match &a.theme {
        Some(n) => Some(watchlist::theme(n).ok_or(format!("unknown theme '{n}'"))?),
        None => None,
    };
    Ok(Session {
        market,
        ledger,
        limits,
        theme,
        kill: Path::new("KILL_SWITCH").exists(),
        journal,
    })
}

impl Session {
    fn context(&self) -> Context<'_> {
        Context {
            market: &self.market,
            scorer: &KeywordScorer,
            theme: self.theme.as_ref(),
            limits: &self.limits,
            book: &self.ledger.book,
            kill_switch: self.kill,
        }
    }
}

fn cmd_analyze(a: &Args) -> Result<(), String> {
    let ticker = a
        .positional
        .get(1)
        .ok_or("analyze needs a ticker, e.g. engine analyze INTC")?;
    let s = open(a)?;
    sample_banner(&s.market);
    let start = Instant::now();
    let result = analyze(&s.context(), ticker)?;
    print_analysis(&result, &s.market, start.elapsed().as_secs_f64() * 1000.0);

    if a.paper {
        match (&result.decision, &result.best) {
            (
                Decision::Approved {
                    contracts,
                    risk,
                    hedge,
                },
                Some(c),
            ) => {
                let fill = PaperBroker.submit(&Order {
                    candidate: c,
                    contracts: *contracts,
                    hedge: hedge.as_ref(),
                })?;
                journal::append(
                    &s.journal,
                    &Entry {
                        analysis: &result,
                        fill: &fill,
                        risk: *risk,
                        hedge: hedge.as_ref(),
                        theme: a.theme.as_deref().unwrap_or(""),
                        as_of: &s.market.as_of_date,
                        source: s.market.source(),
                    },
                )?;
                println!("Paper trade recorded in {}", s.journal.display());
            }
            _ => println!("No paper trade recorded."),
        }
    }
    Ok(())
}

fn cmd_scan(a: &Args) -> Result<(), String> {
    let s = open(a)?;
    sample_banner(&s.market);
    let start = Instant::now();
    let ctx = s.context();
    let mut tickers: Vec<String> = s.market.chains.keys().cloned().collect();
    tickers.sort();
    let mut rows = Vec::new();
    for t in &tickers {
        match analyze(&ctx, t) {
            Ok(r) => rows.push(r),
            Err(e) => println!("skipped {t}: {e}"),
        }
    }
    rows.sort_by(|x, y| {
        let edge = |r: &Analysis| r.best.as_ref().map_or(f64::MIN, |c| c.edge());
        edge(y).total_cmp(&edge(x))
    });
    println!(
        "{:<6} {:<11} {:>6} {:>6} {:>6}  {:<25} {:>6}  decision",
        "ticker", "group", "IV", "rank", "news", "best structure", "edge"
    );
    for r in &rows {
        let (name, edge) = r.best.as_ref().map_or(("-", "-".to_string()), |c| {
            (c.name, format!("{:.1}%", c.edge() * 100.0))
        });
        let decision = match &r.decision {
            Decision::Approved { contracts, .. } => format!("approved, {contracts} contracts"),
            Decision::Blocked(w) => format!("no trade: {w}"),
        };
        println!(
            "{:<6} {:<11} {:>5.1}% {:>6} {:>+6.2}  {:<25} {:>6}  {decision}",
            r.member.ticker,
            r.member.group,
            r.signals.atm_iv * 100.0,
            rank(&r.signals),
            r.news.direction,
            name,
            edge
        );
    }
    println!(
        "\nScanned {} tickers in {:.2} ms",
        rows.len(),
        start.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn cmd_exits(a: &Args) -> Result<(), String> {
    let s = open(a)?;
    sample_banner(&s.market);
    if s.ledger.open.is_empty() {
        println!("No open paper positions.");
        return Ok(());
    }
    let rules = ExitRules::default();
    println!(
        "{:<4} {:<6} {:<25} {:>9} {:>10} {:>7}  action",
        "id", "ticker", "structure", "days left", "open PnL", "of risk"
    );
    let mut closed = 0;
    let mut realized = 0.0;
    for t in &s.ledger.open {
        let reason = exits::evaluate(&rules, t.risk, t.pnl, t.days_left);
        let (pnl, share) = t
            .pnl
            .map_or(("no price".to_string(), "-".to_string()), |v| {
                (format!("{v:+.0}"), format!("{:+.0}%", v / t.risk * 100.0))
            });
        let action = match reason {
            Some(r) => format!("close: {}", r.label()),
            None if t.pnl.is_none() => "hold: cannot price it".to_string(),
            None => "hold".to_string(),
        };
        println!(
            "{:<4} {:<6} {:<25} {:>9} {:>10} {:>7}  {action}",
            t.id, t.ticker, t.structure, t.days_left, pnl, share
        );
        if let (true, Some(r), Some(value)) = (a.paper, reason, t.exit_value) {
            let fill = PaperBroker.close(&CloseOrder {
                value,
                hedge_price: t.hedge_exit,
            })?;
            realized += journal::record_exit(&s.journal, t, &fill, r, &s.market.as_of_date)?;
            closed += 1;
        }
    }
    if a.paper {
        println!("\nClosed {closed} paper trades, realized ${realized:+.0}");
    } else {
        println!("\nPreview only. Add --paper to record these exits.");
    }
    Ok(())
}

fn print_summary(label: &str, trades: &[&ClosedTrade]) {
    let s = journal::summarize(trades.iter().copied());
    println!(
        "{:<14} {:>6} {:>5.0}% {:>10.0} {:>11.0}",
        label,
        s.trades,
        s.wins as f64 / s.trades.max(1) as f64 * 100.0,
        s.pnl,
        s.forecast_ev
    );
}

fn cmd_positions(a: &Args) -> Result<(), String> {
    let s = open(a)?;
    sample_banner(&s.market);
    let l = &s.ledger;
    if l.open.is_empty() {
        println!("No open paper positions.");
    } else {
        println!(
            "{:<4} {:<6} {:<25} {:>9} {:>9} {:>9} {:>10}",
            "id", "ticker", "structure", "contracts", "days left", "risk", "open PnL"
        );
        for p in &l.open {
            let pnl = p.pnl.map_or("no price".to_string(), |v| format!("{v:+.0}"));
            println!(
                "{:<4} {:<6} {:<25} {:>9} {:>9} {:>9.0} {:>10}",
                p.id, p.ticker, p.structure, p.contracts, p.days_left, p.risk, pnl
            );
        }
        println!(
            "\nTotal at risk ${:.0}   open PnL ${:+.0} (marked at exit prices)",
            l.book.total_risk(),
            l.book.unrealized_pnl
        );
    }
    if l.closed.is_empty() {
        return Ok(());
    }

    println!("\nClosed trades");
    println!(
        "{:<4} {:<6} {:<25} {:<12} {:<11} {:>4} {:>9}",
        "id", "ticker", "structure", "exit date", "reason", "days", "PnL"
    );
    for c in &l.closed {
        println!(
            "{:<4} {:<6} {:<25} {:<12} {:<11} {:>4} {:>+9.0}",
            c.id, c.ticker, c.structure, c.exit_date, c.reason, c.days_held, c.pnl
        );
    }

    println!("\nResults against forecast");
    println!(
        "{:<14} {:>6} {:>6} {:>10} {:>11}",
        "", "trades", "wins", "PnL", "forecast EV"
    );
    print_summary("all", &l.closed.iter().collect::<Vec<_>>());
    let mut themes: Vec<&str> = l
        .closed
        .iter()
        .map(|c| c.theme.as_str())
        .filter(|t| !t.is_empty())
        .collect();
    themes.sort_unstable();
    themes.dedup();
    for theme in themes {
        print_summary(
            theme,
            &l.closed
                .iter()
                .filter(|c| c.theme == theme)
                .collect::<Vec<_>>(),
        );
    }
    Ok(())
}

fn cmd_fetch(a: &Args) -> Result<(), String> {
    let mode = if a.delayed {
        Mode::Delayed
    } else {
        Mode::Subscribed
    };
    let tickers: Vec<String> = if a.positional.len() > 1 {
        a.positional[1..].iter().map(|t| t.to_uppercase()).collect()
    } else {
        WATCHLIST.iter().map(|m| m.0.to_string()).collect()
    };
    if let Some(bad) = tickers
        .iter()
        .find(|t| !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'))
    {
        return Err(format!("'{bad}' is not a valid ticker"));
    }
    let source = Ibkr::connect(a.port, mode)?;
    let dir = a.data.clone().unwrap_or_else(|| "data/live".into());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;
    println!(
        "Fetching {} tickers from IBKR ({}). This can take a few minutes...",
        tickers.len(),
        mode.feed()
    );
    let journal = a
        .journal
        .clone()
        .unwrap_or_else(|| journal::default_path(false));
    let cfg = fetch::Config {
        dir: dir.clone(),
        held: journal::held_legs(&journal)?,
        tickers,
        feed: mode.feed().to_string(),
    };
    let r = fetch::run(&source, &cfg, now)?;
    for w in &r.warnings {
        println!("warning: {w}");
    }
    println!(
        "Saved {} to {}: {} option chains, {} quotes, {} news rows",
        r.date,
        r.path.display(),
        r.chains,
        r.quotes,
        r.stories
    );
    println!("Next: engine scan --data {}", dir.join("latest").display());
    Ok(())
}

fn cmd_orders(a: &Args) -> Result<(), String> {
    let path = a
        .orders
        .clone()
        .unwrap_or_else(|| "journal/orders.csv".into());
    let list = orders::read_orders(&path)?;
    if list.is_empty() {
        println!("No orders recorded.");
        return Ok(());
    }
    println!(
        "{:<24} {:<6} {:<24} {:>8} {:>8} {:>9} {:<13}",
        "order", "ticker", "structure", "filled", "limit", "avg fill", "state"
    );
    for o in &list {
        let average = o
            .average_price()
            .map_or("-".to_string(), |p| format!("{p:.2}"));
        println!(
            "{:<24} {:<6} {:<24} {:>8} {:>8.2} {:>9} {:<13}",
            o.intent.id,
            o.intent.ticker,
            o.intent.structure,
            format!("{}/{}", o.filled(), o.intent.quantity),
            o.intent.limit,
            average,
            o.state.name()
        );
    }
    for o in list.iter().filter(|o| o.state.blocks_trading()) {
        println!("\nTRADING STOPPED by {}: {}", o.intent.id, o.detail);
    }
    Ok(())
}

fn labels_path(a: &Args) -> PathBuf {
    a.labels
        .clone()
        .unwrap_or_else(|| "journal/news_labels.csv".into())
}

fn cmd_news_grade(a: &Args) -> Result<(), String> {
    let source = a
        .source
        .clone()
        .unwrap_or_else(|| "data/news_practice.csv".into());
    let headlines = news_check::load_headlines(&source)?;
    let path = labels_path(a);
    let graded = news_check::grade(&headlines, &path, &mut stdin().lock(), &mut stdout())?;
    println!("\nSaved {graded} grades. Run `engine news-check` to see how the scorer did.");
    Ok(())
}

fn percent(v: Option<f64>) -> String {
    v.map_or("n/a".to_string(), |x| format!("{:.0}%", x * 100.0))
}

fn cmd_news_check(a: &Args) -> Result<(), String> {
    let labels = news_check::load_labels(&labels_path(a))?;
    if labels.is_empty() {
        println!("No grades yet. Run `engine news-grade` first.");
        return Ok(());
    }
    let r = news_check::evaluate(&labels, &KeywordScorer);
    let (lo, hi) = r.interval();
    println!("Keyword scorer against your {} grades\n", r.total);
    println!(
        "  agreement        {:.0}%  (95% range {:.0}% to {:.0}%)",
        r.accuracy() * 100.0,
        lo * 100.0,
        hi * 100.0
    );
    println!(
        "  always guessing  {:.0}%  (your most common grade every time)",
        r.baseline() * 100.0
    );
    println!(
        "  kappa            {:.2}  (0 = chance, 1 = perfect)",
        r.kappa()
    );
    println!(
        "  opposite calls   {}  (good called bad, or bad called good)",
        r.opposite_calls()
    );

    println!(
        "\n{:<12} {:>8} {:>8} {:>8}",
        "you \\ scorer", "bad", "neutral", "good"
    );
    for l in LABELS {
        let row = r.matrix[l as usize];
        println!("{:<12} {:>8} {:>8} {:>8}", l.name(), row[0], row[1], row[2]);
    }

    println!("\n{:<10} {:>10} {:>8}", "", "precision", "recall");
    for l in [Label::Good, Label::Bad] {
        println!(
            "{:<10} {:>10} {:>8}",
            l.name(),
            percent(r.precision(l)),
            percent(r.recall(l))
        );
    }

    if !r.misses.is_empty() {
        println!("\nWorst disagreements");
        for m in r.misses.iter().take(10) {
            println!(
                "  you {:<7} scorer {:<7} {:+.2}  {}",
                m.yours.name(),
                m.scorer.name(),
                m.score,
                m.headline
            );
        }
    }
    if r.total < news_check::ENOUGH_LABELS {
        println!(
            "\nOnly {} grades so far, so treat this as a rough guide. Aim for {}.",
            r.total,
            news_check::ENOUGH_LABELS
        );
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let a = parse(&raw)?;
    if a.help {
        println!("{USAGE}");
        return Ok(());
    }
    match a.positional.first().map(String::as_str) {
        Some("analyze") => cmd_analyze(&a),
        Some("scan") => cmd_scan(&a),
        Some("news-grade") => cmd_news_grade(&a),
        Some("news-check") => cmd_news_check(&a),
        Some("orders") => cmd_orders(&a),
        Some("fetch") => cmd_fetch(&a),
        Some("exits") => cmd_exits(&a),
        Some("positions") => cmd_positions(&a),
        Some("gen-sample") => {
            let dir = a.data.clone().unwrap_or_else(|| "data/sample".into());
            sample::generate(&dir, a.seed.unwrap_or(42), a.advance)?;
            println!(
                "Wrote synthetic data for {} stocks to {}",
                WATCHLIST.len(),
                dir.display()
            );
            Ok(())
        }
        None | Some("help") => {
            println!("{USAGE}");
            Ok(())
        }
        // A typo must not exit successfully, or a script running it would
        // think the step worked.
        Some(other) => Err(format!(
            "unknown command '{other}'. Run `engine help` to see the commands."
        )),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
