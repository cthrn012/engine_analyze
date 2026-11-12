//! Tracks orders from intent to completion. This file knows nothing about any
//! real brokerage: it talks to an `OrderGateway`, and in phase 1 the only
//! gateway is a fake one in the tests.
//!
//! Every change is appended to a log before anything else happens, and current
//! state is rebuilt by replaying that log. An order's intent is saved before it
//! is sent, so a crash can't lose track of it, and a restart never resends:
//! it asks the broker what happened instead.
use crate::csv::{one_line, quote, Row, Table};
use crate::dates::format_date;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_ORDERS_PER_DAY: usize = 20;
pub const MAX_CONTRACTS: u32 = 20;
const HEADER: &str =
    "at,order_id,event,ticker,structure,legs,quantity,limit,broker_id,fill_id,fill_qty,fill_price,detail";

/// There is deliberately no market order type: every order has a limit price.
/// A negative limit is a credit, which spreads can have.
#[derive(Clone, Debug, PartialEq)]
pub struct Intent {
    pub id: String,
    pub ticker: String,
    pub structure: String,
    pub legs: String,
    pub quantity: u32,
    pub limit: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Planned,
    Sent,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    /// We can't tell whether the broker has it. Checking may clear this.
    Unknown,
    /// The broker's account disagrees with ours. This needs a person.
    Mismatch,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Planned => "planned",
            State::Sent => "sent",
            State::PartiallyFilled => "partly filled",
            State::Filled => "filled",
            State::Cancelled => "cancelled",
            State::Rejected => "rejected",
            State::Unknown => "unknown",
            State::Mismatch => "mismatch",
        }
    }

    pub fn is_final(self) -> bool {
        matches!(self, State::Filled | State::Cancelled | State::Rejected)
    }

    /// New orders stop while any order is in one of these states.
    pub fn blocks_trading(self) -> bool {
        matches!(self, State::Unknown | State::Mismatch)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub fill_id: String,
    pub quantity: u32,
    pub price: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Planned(Intent),
    Sent { broker_id: String },
    Fill(Fill),
    Cancelled { detail: String },
    Rejected { detail: String },
    Unknown { detail: String },
    Mismatch { detail: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Order {
    pub intent: Intent,
    pub state: State,
    pub broker_id: Option<String>,
    pub fills: Vec<Fill>,
    pub detail: String,
    pub planned_at: i64,
}

impl Order {
    pub fn filled(&self) -> u32 {
        self.fills.iter().map(|f| f.quantity).sum()
    }

    pub fn average_price(&self) -> Option<f64> {
        let qty = self.filled();
        (qty > 0).then(|| {
            self.fills
                .iter()
                .map(|f| f.price * f.quantity as f64)
                .sum::<f64>()
                / qty as f64
        })
    }

    fn working_state(&self) -> State {
        if self.filled() > 0 {
            State::PartiallyFilled
        } else {
            State::Sent
        }
    }

    fn apply(&mut self, event: &Event) -> Result<(), String> {
        let bad =
            |s: State, what: &str| Err(format!("can't record {what} on a {} order", s.name()));
        match event {
            Event::Planned(_) => return Err("order was planned twice".into()),
            Event::Sent { broker_id } => {
                if !matches!(self.state, State::Planned | State::Unknown) {
                    return bad(self.state, "a send");
                }
                self.broker_id = Some(broker_id.clone());
                self.state = self.working_state();
            }
            Event::Fill(f) => {
                if !matches!(self.state, State::Sent | State::PartiallyFilled) {
                    return bad(self.state, "a fill");
                }
                if self.fills.iter().any(|x| x.fill_id == f.fill_id) {
                    return Err(format!("fill {} was recorded twice", f.fill_id));
                }
                if self.filled() + f.quantity > self.intent.quantity {
                    return Err("fills exceed the order size".into());
                }
                self.fills.push(f.clone());
                self.state = if self.filled() == self.intent.quantity {
                    State::Filled
                } else {
                    State::PartiallyFilled
                };
            }
            Event::Cancelled { detail } => {
                if self.state.is_final() {
                    return bad(self.state, "a cancel");
                }
                self.state = State::Cancelled;
                self.detail = detail.clone();
            }
            Event::Rejected { detail } => {
                if !matches!(self.state, State::Planned | State::Sent | State::Unknown) {
                    return bad(self.state, "a rejection");
                }
                self.state = State::Rejected;
                self.detail = detail.clone();
            }
            Event::Unknown { detail } => {
                self.state = State::Unknown;
                self.detail = detail.clone();
            }
            Event::Mismatch { detail } => {
                self.state = State::Mismatch;
                self.detail = detail.clone();
            }
        }
        Ok(())
    }
}

fn encode(at: i64, id: &str, event: &Event) -> String {
    let blank = String::new;
    // ticker, structure, legs, quantity, limit, broker, fill id, fill qty, fill price, detail
    let (name, c): (&str, [String; 10]) = match event {
        Event::Planned(i) => (
            "planned",
            [
                i.ticker.clone(),
                i.structure.clone(),
                i.legs.clone(),
                i.quantity.to_string(),
                i.limit.to_string(),
                blank(),
                blank(),
                blank(),
                blank(),
                blank(),
            ],
        ),
        Event::Sent { broker_id } => (
            "sent",
            [
                blank(),
                blank(),
                blank(),
                blank(),
                blank(),
                broker_id.clone(),
                blank(),
                blank(),
                blank(),
                blank(),
            ],
        ),
        Event::Fill(f) => (
            "fill",
            [
                blank(),
                blank(),
                blank(),
                blank(),
                blank(),
                blank(),
                f.fill_id.clone(),
                f.quantity.to_string(),
                f.price.to_string(),
                blank(),
            ],
        ),
        Event::Cancelled { detail } => ("cancelled", detail_row(detail)),
        Event::Rejected { detail } => ("rejected", detail_row(detail)),
        Event::Unknown { detail } => ("unknown", detail_row(detail)),
        Event::Mismatch { detail } => ("mismatch", detail_row(detail)),
    };
    let mut cells = vec![at.to_string(), id.to_string(), name.to_string()];
    cells.extend(c);
    cells
        .iter()
        .map(|c| quote(&one_line(c)))
        .collect::<Vec<_>>()
        .join(",")
}

fn detail_row(detail: &str) -> [String; 10] {
    let mut row: [String; 10] = Default::default();
    row[9] = detail.to_string();
    row
}

fn decode(r: &Row) -> Result<(i64, String, Event), String> {
    let id = r.get("order_id")?.to_string();
    let detail = || r.get("detail").map(String::from);
    let event = match r.get("event")? {
        "planned" => Event::Planned(Intent {
            id: id.clone(),
            ticker: r.get("ticker")?.to_string(),
            structure: r.get("structure")?.to_string(),
            legs: r.get("legs")?.to_string(),
            quantity: r.num("quantity")?,
            limit: r.num("limit")?,
        }),
        "sent" => Event::Sent {
            broker_id: r.get("broker_id")?.to_string(),
        },
        "fill" => Event::Fill(Fill {
            fill_id: r.get("fill_id")?.to_string(),
            quantity: r.num("fill_qty")?,
            price: r.num("fill_price")?,
        }),
        "cancelled" => Event::Cancelled { detail: detail()? },
        "rejected" => Event::Rejected { detail: detail()? },
        "unknown" => Event::Unknown { detail: detail()? },
        "mismatch" => Event::Mismatch { detail: detail()? },
        other => return Err(r.err(&format!("unknown event '{other}'"))),
    };
    Ok((r.num("at")?, id, event))
}

fn replay(name: &str, text: &str) -> Result<Vec<Order>, String> {
    let mut orders: Vec<Order> = Vec::new();
    if text.trim().is_empty() {
        return Ok(orders);
    }
    for r in Table::parse(name, text)?.iter() {
        let (at, id, event) = decode(&r)?;
        match (&event, orders.iter_mut().find(|o| o.intent.id == id)) {
            (Event::Planned(intent), None) => orders.push(Order {
                intent: intent.clone(),
                state: State::Planned,
                broker_id: None,
                fills: Vec::new(),
                detail: String::new(),
                planned_at: at,
            }),
            (_, Some(order)) => order.apply(&event).map_err(|e| r.err(&e))?,
            (_, None) => return Err(r.err(&format!("event for unplanned order {id}"))),
        }
    }
    Ok(orders)
}

/// Text up to the last complete line. A crash mid-write can leave a partial
/// final line, which was never a recorded event.
fn complete_lines(text: &str) -> &str {
    text.rfind('\n').map_or("", |i| &text[..=i])
}

/// Reads orders without changing the file, for reports.
pub fn read_orders(path: &Path) -> Result<Vec<Order>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    replay(&path.display().to_string(), complete_lines(&text))
}

/// The append-only record. An event is on disk before this returns.
pub struct OrderLog {
    path: PathBuf,
    orders: Vec<Order>,
}

impl OrderLog {
    pub fn open(path: &Path) -> Result<OrderLog, String> {
        let io = |e: std::io::Error| e.to_string();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(io)?;
        }
        if !path.exists() {
            let mut f = File::create(path).map_err(io)?;
            writeln!(f, "{HEADER}").map_err(io)?;
            f.sync_all().map_err(io)?;
        }
        let text = fs::read_to_string(path).map_err(io)?;
        let keep = complete_lines(&text);
        if keep.len() < text.len() {
            // Later appends would otherwise join onto the partial line.
            OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(io)?
                .set_len(keep.len() as u64)
                .map_err(io)?;
        }
        let orders = replay(&path.display().to_string(), keep)?;
        Ok(OrderLog {
            path: path.to_path_buf(),
            orders,
        })
    }

    pub fn orders(&self) -> &[Order] {
        &self.orders
    }

    pub fn get(&self, id: &str) -> Option<&Order> {
        self.orders.iter().find(|o| o.intent.id == id)
    }

    fn record(&mut self, at: i64, id: &str, event: Event) -> Result<(), String> {
        // Check the change is valid first, so the file never holds a bad event.
        let existing = self.orders.iter().position(|o| o.intent.id == id);
        let updated = match (&event, existing) {
            (Event::Planned(intent), None) => Order {
                intent: intent.clone(),
                state: State::Planned,
                broker_id: None,
                fills: Vec::new(),
                detail: String::new(),
                planned_at: at,
            },
            (_, Some(i)) => {
                let mut o = self.orders[i].clone();
                o.apply(&event)?;
                o
            }
            (_, None) => return Err(format!("no planned order {id}")),
        };
        let mut f = OpenOptions::new()
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{}", encode(at, id, &event)).map_err(|e| e.to_string())?;
        f.sync_data().map_err(|e| e.to_string())?;
        match existing {
            Some(i) => self.orders[i] = updated,
            None => self.orders.push(updated),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum GatewayError {
    /// The broker definitely refused it.
    Rejected(String),
    /// It may or may not have arrived, such as after a timeout.
    Uncertain(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum BrokerStatus {
    Working,
    Filled,
    Cancelled,
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrokerOrder {
    pub broker_id: String,
    pub status: BrokerStatus,
    pub fills: Vec<Fill>,
}

pub trait OrderGateway {
    /// Whether the connected account is a paper account.
    fn is_paper(&mut self) -> Result<bool, String>;
    /// Sends an order and returns the broker's ID for it.
    fn send(&mut self, intent: &Intent) -> Result<String, GatewayError>;
    fn cancel(&mut self, broker_id: &str) -> Result<(), String>;
    /// Finds an order by our ID, which travels with the order.
    fn lookup(&mut self, order_id: &str) -> Result<Option<BrokerOrder>, String>;
}

pub struct OrderManager<'a> {
    log: OrderLog,
    gateway: &'a mut dyn OrderGateway,
}

impl<'a> OrderManager<'a> {
    pub fn new(log: OrderLog, gateway: &'a mut dyn OrderGateway) -> Self {
        OrderManager { log, gateway }
    }

    pub fn orders(&self) -> &[Order] {
        self.log.orders()
    }

    pub fn get(&self, id: &str) -> Option<&Order> {
        self.log.get(id)
    }

    /// Why new orders are blocked, if they are.
    pub fn halt_reason(&self) -> Option<String> {
        self.orders()
            .iter()
            .find(|o| o.state.blocks_trading())
            .map(|o| {
                format!(
                    "order {} is in state '{}': {}",
                    o.intent.id,
                    o.state.name(),
                    o.detail
                )
            })
    }

    fn orders_on(&self, day: i64) -> usize {
        self.orders()
            .iter()
            .filter(|o| o.planned_at.div_euclid(86400) == day)
            .count()
    }

    /// An ID that is unique in this log and readable in the broker's records.
    pub fn next_id(&self, ticker: &str, now: i64) -> String {
        format!(
            "{}-{ticker}-{}",
            format_date(now.div_euclid(86400)),
            self.orders().len() + 1
        )
    }

    fn check(&mut self, intent: &Intent, now: i64, kill_switch: bool) -> Result<(), String> {
        if kill_switch {
            return Err("kill switch is on".into());
        }
        if let Some(why) = self.halt_reason() {
            return Err(format!("trading is stopped: {why}"));
        }
        if intent.id.is_empty() || intent.id.contains([',', '"', '\n']) {
            return Err("order ID is not usable".into());
        }
        if self.get(&intent.id).is_some() {
            return Err(format!("order {} already exists", intent.id));
        }
        if intent.quantity == 0 || intent.quantity > MAX_CONTRACTS {
            return Err(format!("quantity must be 1 to {MAX_CONTRACTS}"));
        }
        if !intent.limit.is_finite() || intent.limit == 0.0 {
            return Err("every order needs a limit price".into());
        }
        if self.orders_on(now.div_euclid(86400)) >= MAX_ORDERS_PER_DAY {
            return Err(format!(
                "daily limit of {MAX_ORDERS_PER_DAY} orders reached"
            ));
        }
        match self.gateway.is_paper() {
            Ok(true) => Ok(()),
            Ok(false) => Err("the connected account is not a paper account".into()),
            Err(e) => Err(format!("could not confirm a paper account: {e}")),
        }
    }

    /// Records the intent. Nothing has been sent when this returns.
    pub fn plan(&mut self, intent: Intent, now: i64, kill_switch: bool) -> Result<(), String> {
        self.check(&intent, now, kill_switch)?;
        let id = intent.id.clone();
        self.log.record(now, &id, Event::Planned(intent))
    }

    /// Sends a planned order, exactly once. Any uncertainty is recorded as
    /// such and never retried here.
    pub fn send(&mut self, id: &str, now: i64, kill_switch: bool) -> Result<State, String> {
        let order = self.get(id).ok_or_else(|| format!("no order {id}"))?;
        if order.state != State::Planned {
            return Err(format!("order {id} is already {}", order.state.name()));
        }
        let intent = order.intent.clone();
        if kill_switch {
            return Err("kill switch is on".into());
        }
        match self.gateway.is_paper() {
            Ok(true) => {}
            Ok(false) => return Err("the connected account is not a paper account".into()),
            Err(e) => return Err(format!("could not confirm a paper account: {e}")),
        }
        let event = match self.gateway.send(&intent) {
            Ok(broker_id) => Event::Sent { broker_id },
            Err(GatewayError::Rejected(detail)) => Event::Rejected { detail },
            Err(GatewayError::Uncertain(detail)) => Event::Unknown { detail },
        };
        self.log.record(now, id, event)?;
        Ok(self.get(id).map_or(State::Unknown, |o| o.state))
    }

    /// Plans and sends in one step.
    pub fn submit(&mut self, intent: Intent, now: i64, kill_switch: bool) -> Result<State, String> {
        let id = intent.id.clone();
        self.plan(intent, now, kill_switch)?;
        self.send(&id, now, kill_switch)
    }

    /// Asks the broker to cancel. The result arrives through `sync`. This
    /// still works with the kill switch on, because it reduces risk.
    pub fn cancel(&mut self, id: &str) -> Result<(), String> {
        let order = self.get(id).ok_or_else(|| format!("no order {id}"))?;
        let broker_id = order
            .broker_id
            .clone()
            .ok_or("the broker has no ID for this order yet")?;
        if order.state.is_final() {
            return Err(format!("order {id} is already {}", order.state.name()));
        }
        self.gateway.cancel(&broker_id)
    }

    /// Brings every open order up to date with the broker. Run this after a
    /// restart before anything else.
    pub fn sync(&mut self, now: i64) -> Result<(), String> {
        let open: Vec<String> = self
            .orders()
            .iter()
            .filter(|o| !o.state.is_final() && o.state != State::Mismatch)
            .map(|o| o.intent.id.clone())
            .collect();
        for id in open {
            match self.gateway.lookup(&id)? {
                Some(view) => self.absorb(&id, view, now)?,
                None => self.handle_missing(&id, now)?,
            }
        }
        Ok(())
    }

    fn handle_missing(&mut self, id: &str, now: i64) -> Result<(), String> {
        let event = match self.get(id).map(|o| o.state) {
            // It stopped before sending. It stays closed: sending again has to
            // be a new, deliberate order.
            Some(State::Planned) => Event::Cancelled {
                detail: "never reached the broker".into(),
            },
            Some(State::Sent | State::PartiallyFilled) => Event::Mismatch {
                detail: "the broker no longer reports this order".into(),
            },
            // It could still be in flight, so keep waiting.
            _ => return Ok(()),
        };
        self.log.record(now, id, event)
    }

    fn absorb(&mut self, id: &str, view: BrokerOrder, now: i64) -> Result<(), String> {
        if matches!(
            self.get(id).map(|o| o.state),
            Some(State::Planned | State::Unknown)
        ) {
            let broker_id = view.broker_id.clone();
            self.log.record(now, id, Event::Sent { broker_id })?;
        }
        for fill in view.fills {
            let Some(order) = self.get(id) else { continue };
            if order.fills.iter().any(|f| f.fill_id == fill.fill_id) {
                continue;
            }
            if order.filled() + fill.quantity > order.intent.quantity {
                let detail = "the broker reports more fills than the order size".to_string();
                return self.log.record(now, id, Event::Mismatch { detail });
            }
            if order.state.is_final() {
                let detail = "the broker reports a fill on a closed order".to_string();
                return self.log.record(now, id, Event::Mismatch { detail });
            }
            self.log.record(now, id, Event::Fill(fill))?;
        }
        let Some(order) = self.get(id) else {
            return Ok(());
        };
        let event = match view.status {
            BrokerStatus::Working => return Ok(()),
            BrokerStatus::Filled if order.state == State::Filled => return Ok(()),
            BrokerStatus::Filled => Event::Mismatch {
                detail: "the broker says filled, but fills are missing".into(),
            },
            BrokerStatus::Cancelled => Event::Cancelled {
                detail: "cancelled at the broker".into(),
            },
            BrokerStatus::Rejected(detail) if order.filled() == 0 => Event::Rejected { detail },
            BrokerStatus::Rejected(detail) => Event::Mismatch { detail },
        };
        if order.state.is_final() {
            return Ok(());
        }
        self.log.record(now, id, event)
    }

    /// Closes an order a person has checked by hand, which lets trading resume.
    pub fn resolve(&mut self, id: &str, note: &str, now: i64) -> Result<(), String> {
        let order = self.get(id).ok_or_else(|| format!("no order {id}"))?;
        if !order.state.blocks_trading() {
            return Err(format!("order {id} doesn't need resolving"));
        }
        self.log.record(
            now,
            id,
            Event::Cancelled {
                detail: format!("resolved by hand: {note}"),
            },
        )
    }
}
