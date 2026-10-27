//! the operator interface: a [hyperb](https://github.com/andunieee/hyperb) app, the only way
//! to run the server. everything the operator does goes through here: the books, the audit,
//! moderation, the daily rent and orders.
//!
//! [transport] authenticates the operator by the key of their hyperb identity. a session
//! ([OperatorSession]) turns the lines the renderer sends into the lines that update the
//! interface.

pub mod transport;

use std::sync::Arc;

use hypereffect::{Action, Element, Handler, Outbox, RendererMessage, Value, cel, json, ui};

use crate::app::{App, AppError, Result, now, parse_hash, parse_pubkey};
use crate::audit::{Audit, Level};
use crate::config::{PaymentsConfig, ProviderConfig};
use crate::store::{OrderKind, ProviderPrices};

const ROWS: usize = 12;

pub struct OperatorSession {
    app: Arc<App>,
    /// the ids of the parts of the interface that are drawn again as a whole when their data
    /// changes
    audit_rows: Vec<String>,
    entry_rows: Vec<String>,
    found_rows: Vec<String>,
    ban_rows: Vec<String>,
    order_rows: Vec<String>,
}

/// what the select menus show
fn menu(values: &[(String, String)]) -> Vec<Value> {
    values.iter().map(|(v, l)| json!({"value": v, "label": l})).collect()
}

fn provider_name(p: &ProviderConfig) -> String {
    format!("{} · {:?}", p.id, p.kind).to_lowercase()
}

fn replace(out: &mut Outbox, ids: &mut Vec<String>, new: Vec<Element>) {
    for id in ids.drain(..) {
        out.remove(id);
    }
    for e in new {
        ids.push(e.id.clone());
        out.element(e);
    }
}

fn say(out: &mut Outbox, kind: &str, text: impl Into<String>) {
    out.signal(json!({"feedback": {"kind": kind, "text": text.into()}}));
}

/// a signal as trimmed text; numbers are written out
fn text(action: &Action, path: &str) -> String {
    match action.signal(path) {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn flag(action: &Action, path: &str) -> bool {
    action.signal(path).and_then(Value::as_bool).unwrap_or(false)
}

fn bad(text: &str) -> AppError {
    AppError::BadRequest(text.into())
}

fn sats(n: impl Into<i128>) -> String {
    let n: i128 = n.into();
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}{out} sats", if n < 0 { "-" } else { "" })
}

fn short(hex: &str) -> String {
    format!("{}…{}", &hex[..8.min(hex.len())], &hex[hex.len().saturating_sub(4)..])
}

// dates, without a calendar library (https://howardhinnant.github.io/date_algorithms.html)

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// YYYY-MM-DD as unix seconds at the start of that day (UTC)
pub fn parse_date(s: &str) -> Option<u64> {
    let mut it = s.trim().split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(2000..=2200).contains(&y) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    // refuse the 31st of February and friends
    (civil_from_days(days) == (y, m, d)).then(|| (days * 86400) as u64)
}

pub fn format_date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

impl OperatorSession {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            audit_rows: vec![],
            entry_rows: vec![],
            found_rows: vec![],
            ban_rows: vec![],
            order_rows: vec![],
        }
    }

    /// the lines the renderer sent, and what the interface does about them
    pub async fn handle(&mut self, lines: &str) -> String {
        let mut out = Outbox::new();
        for line in lines.lines().filter(|l| !l.trim().is_empty()) {
            match RendererMessage::parse(line) {
                Ok(RendererMessage::Hello { .. }) => self.hello(&mut out).await,
                Ok(RendererMessage::Action(a)) => self.action(&mut out, &a).await,
                Err(_) => {}
            }
        }
        out.take()
    }

    async fn hello(&mut self, out: &mut Outbox) {
        let app = self.app.clone();
        let providers: Vec<_> = app
            .config
            .providers
            .iter()
            .map(|p| (p.id.to_string(), provider_name(p)))
            .collect();
        let first = providers.first().map(|p| p.0.clone()).unwrap_or_default();
        let currency = &app.config.fiat.currency;
        let close = json!({"confirm": {"open": false}});

        out.signal(json!({
            "feedback": {"kind": "info", "text": ""},
            "audit": {"days": "30"},
            "expense": {"provider": first, "amount": "", "date": format_date(now()), "reference": "", "note": ""},
            "sale": {"sats": "", "fiat": "", "reference": "", "note": ""},
            "rate": {"value": ""},
            "prices": {"provider": first, "storage": "", "egress": ""},
            "mod": {"hash": "", "pubkey": "", "purge": false},
            "menu": {
                "providers": menu(&providers),
                "days": menu(&[("7".into(), "Last 7 days".into()), ("30".into(), "Last 30 days".into()), ("90".into(), "Last 90 days".into())]),
            },
            "confirm": {"open": false, "text": "", "action": "", "arg": ""},
        }));

        out.element(ui::column("main"));
        out.element(ui::text("title").parent("main").variant("h1").text("garden"));
        out.element(ui::text("subtitle").parent("main").variant("caption").text(format!("Operator interface of {}", app.config.public_url)));
        out.element(
            ui::notice("feedback")
                .parent("main")
                .variant(cel("s.feedback.kind"))
                .text(cel("s.feedback.text"))
                .visible(cel("s.feedback.text != ''")),
        );
        out.element(ui::tabs("tabs").parent("main"));

        // the audit
        out.element(ui::tab("audit_tab").parent("tabs").label("Audit"));
        out.element(ui::row("audit_controls").parent("audit_tab").wrap(true));
        out.element(
            ui::select("audit_days")
                .parent("audit_controls")
                .bind("audit.days")
                .options(cel("s.menu.days")),
        );
        out.element(
            ui::button("audit_run")
                .parent("audit_controls")
                .label("Run the audit")
                .variant("primary")
                .on_press(Handler::action("audit").indicator("_busy")),
        );
        out.element(ui::progress("audit_busy").parent("audit_tab").variant("spinner").visible(cel("_busy == true")));
        out.element(ui::column("audit_out").parent("audit_tab"));

        // the books
        out.element(ui::tab("books_tab").parent("tabs").label("Books"));
        out.element(
            ui::text("books_help").parent("books_tab").variant("caption").text(
                "Everything here is optional. The audit uses what you enter to check that the money adds up. Entering the same reference twice does nothing.",
            ),
        );

        out.element(ui::card("expense_card").parent("books_tab").title("A bill you paid to a provider"));
        out.element(ui::select("expense_provider").parent("expense_card").label("Provider").bind("expense.provider").options(cel("s.menu.providers")));
        out.element(ui::input("expense_amount").parent("expense_card").label(format!("Amount ({currency})")).bind("expense.amount"));
        out.element(ui::input("expense_date").parent("expense_card").label("Date of the bill (YYYY-MM-DD)").bind("expense.date"));
        out.element(ui::input("expense_reference").parent("expense_card").label("Reference, e.g. the invoice number").bind("expense.reference"));
        out.element(ui::input("expense_note").parent("expense_card").label("Note (optional)").bind("expense.note"));
        out.element(ui::button("expense_go").parent("expense_card").label("Record the bill").variant("primary").on_press(Handler::action("expense")));

        out.element(ui::card("sale_card").parent("books_tab").title("Sats you sold"));
        out.element(ui::input("sale_sats").parent("sale_card").label("Sats sold").bind("sale.sats"));
        out.element(ui::input("sale_fiat").parent("sale_card").label(format!("{currency} received")).bind("sale.fiat"));
        out.element(ui::input("sale_reference").parent("sale_card").label("Reference, e.g. the id of the trade").bind("sale.reference"));
        out.element(ui::input("sale_note").parent("sale_card").label("Note (optional)").bind("sale.note"));
        out.element(ui::button("sale_go").parent("sale_card").label("Record the sale").variant("primary").on_press(Handler::action("sale")));

        out.element(ui::card("rate_card").parent("books_tab").title("Price of bitcoin"));
        out.element(ui::text("rate_now").parent("rate_card").variant("caption").text(self.rate_text()));
        out.element(ui::input("rate_value").parent("rate_card").label(format!("{currency} for 1 BTC")).bind("rate.value"));
        out.element(ui::button("rate_go").parent("rate_card").label("Save the price").on_press(Handler::action("set_rate")));

        out.element(ui::card("prices_card").parent("books_tab").title("What a provider charges"));
        out.element(
            ui::select("prices_provider")
                .parent("prices_card")
                .label("Provider")
                .bind("prices.provider")
                .options(cel("s.menu.providers"))
                .on_change(Handler::action("load_prices")),
        );
        out.element(ui::input("prices_storage").parent("prices_card").label(format!("{currency} per GB per month of storage")).bind("prices.storage"));
        out.element(ui::input("prices_egress").parent("prices_card").label(format!("{currency} per GB downloaded")).bind("prices.egress"));
        out.element(ui::button("prices_go").parent("prices_card").label("Save the prices").on_press(Handler::action("set_prices")));

        out.element(ui::card("entries_card").parent("books_tab").title("Recent entries"));

        // moderation
        out.element(ui::tab("mod_tab").parent("tabs").label("Moderation"));
        out.element(ui::input("mod_hash").parent("mod_tab").label("File (sha256)").bind("mod.hash"));
        out.element(ui::row("mod_file_buttons").parent("mod_tab").wrap(true));
        out.element(ui::button("mod_who").parent("mod_file_buttons").label("Who uploaded it?").on_press(Handler::action("uploaders")));
        out.element(ui::button("mod_purge").parent("mod_file_buttons").label("Remove it for good").variant("failure").on_press(Handler::action("ask_purge")));
        out.element(ui::input("mod_pubkey").parent("mod_tab").label("Account (pubkey in hex)").bind("mod.pubkey"));
        out.element(ui::checkbox("mod_everything").parent("mod_tab").label("Also remove everything it uploaded").bind("mod.purge"));
        out.element(ui::row("mod_account_buttons").parent("mod_tab").wrap(true));
        out.element(ui::button("mod_files").parent("mod_account_buttons").label("What did it upload?").on_press(Handler::action("files_of")));
        out.element(ui::button("mod_ban").parent("mod_account_buttons").label("Ban it").variant("failure").on_press(Handler::action("ask_ban")));
        out.element(ui::column("mod_found").parent("mod_tab"));
        out.element(ui::card("bans_card").parent("mod_tab").title("Banned"));

        // rent and orders
        out.element(ui::tab("ops_tab").parent("tabs").label("Rent and orders"));
        out.element(
            ui::text("ops_help").parent("ops_tab").variant("caption").text(
                "The rent of every file is taken once a day by itself. Running it again the same day changes nothing.",
            ),
        );
        out.element(ui::row("ops_buttons").parent("ops_tab").wrap(true));
        out.element(ui::button("ops_sweep").parent("ops_buttons").label("Take today's rent now").on_press(Handler::action("sweep").indicator("_busy")));
        out.element(ui::button("ops_orders").parent("ops_buttons").label("Show waiting orders").on_press(Handler::action("orders")));
        out.element(ui::card("orders_card").parent("ops_tab").title("Orders waiting for their payment"));

        // are you sure
        out.element(
            ui::dialog("confirm")
                .open(cel("s.confirm.open"))
                .title("Are you sure?")
                .on_dismiss(Handler::patch(close.clone())),
        );
        out.element(ui::text("confirm_text").parent("confirm").text(cel("s.confirm.text")));
        out.element(ui::row("confirm_buttons").parent("confirm"));
        out.element(
            ui::button("confirm_yes")
                .parent("confirm_buttons")
                .label("Yes, do it")
                .variant("failure")
                .on_press(Handler::action("confirmed").with_patch(close.clone())),
        );
        out.element(ui::button("confirm_no").parent("confirm_buttons").label("Cancel").variant("plain").on_press(Handler::patch(close)));

        if let Err(e) = self.show_entries(out).await.and(self.show_bans(out)).and(self.show_orders(out)) {
            say(out, "failure", e.to_string());
        }
    }

    async fn action(&mut self, out: &mut Outbox, a: &Action) {
        say(out, "info", "");
        let done = match a.name.as_str() {
            "audit" => self.run_audit(out, a).await,
            "expense" => self.expense(out, a).await,
            "sale" => self.sale(out, a).await,
            "set_rate" => self.set_rate(out, a),
            "set_prices" => self.set_prices(out, a),
            "load_prices" => self.load_prices(out, a),
            "uploaders" => self.uploaders(out, a).await,
            "files_of" => self.files_of(out, a).await,
            "ask_purge" => self.ask_purge(out, a),
            "ask_ban" => self.ask_ban(out, a),
            "confirmed" => self.confirmed(out, a).await,
            "sweep" => self.sweep(out).await,
            "orders" => self.show_orders(out),
            "mock_paid" => self.mock_paid(out, a).await,
            _ => Ok(()),
        };
        if let Err(e) = done {
            say(out, "failure", e.to_string());
        }
    }

    // the audit

    async fn run_audit(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let days = text(a, "audit.days").parse().unwrap_or(30u64).clamp(1, 365);
        let audit = self.app.audit(days).await?;
        let rows = self.audit_elements(&audit);
        replace(out, &mut self.audit_rows, rows);
        Ok(())
    }

    fn audit_elements(&self, a: &Audit) -> Vec<Element> {
        let fiat = |minor: i64| self.app.config.fiat.format(minor);
        let opt = |v: Option<i64>| v.map(fiat).unwrap_or_else(|| "—".into());
        let mut els = vec![];

        let card = |els: &mut Vec<Element>, id: &str, title: String, lines: Vec<String>| {
            els.push(ui::card(id).parent("audit_out").title(title));
            for (i, l) in lines.into_iter().enumerate() {
                els.push(ui::text(format!("{id}_{i}")).parent(id).text(l));
            }
        };

        els.push(ui::text("a_period").parent("audit_out").variant("caption").text(format!(
            "{} to {}",
            format_date(a.from_day * 86400),
            format_date(a.to_day * 86400)
        )));
        if a.warnings.is_empty() {
            els.push(ui::notice("a_ok").parent("audit_out").variant("success").text("Nothing to point out."));
        }
        for (i, w) in a.warnings.iter().enumerate() {
            let variant = match w.level {
                Level::Info => "info",
                Level::Warning => "warning",
                Level::Failure => "failure",
            };
            els.push(ui::notice(format!("a_warning_{i}")).parent("audit_out").variant(variant).text(w.text.clone()));
        }

        let i = &a.integrity;
        card(
            &mut els,
            "a_integrity",
            if i.difference == 0 { "The ledger adds up".into() } else { "The ledger does NOT add up".into() },
            vec![
                format!("Received: {}", sats(i.received)),
                format!("In accounts: {} · in files: {}", sats(i.in_accounts), sats(i.in_files)),
                format!("Earned as rent: {} · unclaimed: {}", sats(i.earned), sats(i.unclaimed)),
            ],
        );

        match &a.wallet {
            Some(w) => card(
                &mut els,
                "a_wallet",
                if w.difference == 0 { "The wallet matches".into() } else { "The wallet doesn't match".into() },
                vec![
                    format!("The wallet holds {}", sats(w.actual)),
                    format!("The books say {} (including {} waiting in open orders)", sats(w.expected), sats(w.waiting_for_orders)),
                    format!("Difference: {}", sats(w.difference)),
                ],
            ),
            None => card(&mut els, "a_wallet", "The wallet".into(), vec!["This kind of payments can't tell what the wallet holds.".into()]),
        }

        let p = &a.position;
        let rate = match (p.rate_minor_per_btc, p.rate_source) {
            (Some(r), Some(src)) => format!("{} for 1 BTC ({src})", fiat(r.min(i64::MAX as u64) as i64)),
            _ => "none".into(),
        };
        card(
            &mut els,
            "a_position",
            "What you have".into(),
            vec![
                format!("Earned and not sold: {} (worth {})", sats(p.unsold_sats), opt(p.unsold_value_minor)),
                format!("Sold so far: {}", sats(p.sold_sats)),
                format!("In fiat, from sales minus bills: {}", fiat(p.fiat_on_hand_minor)),
                format!("Price used: {rate}"),
                format!("All together: {}", opt(p.net_minor)),
            ],
        );

        let o = &a.obligations;
        card(
            &mut els,
            "a_obligations",
            "What you owe the files".into(),
            vec![
                format!("{} open files, {} bytes", o.open_files, o.open_bytes),
                format!("They will pay {} more in rent (worth {})", sats(o.in_open_files), opt(o.future_rent_minor)),
                format!("Keeping them until then costs {} at the prices you entered", opt(o.projected_cost_minor)),
                format!("After that you would have {}", opt(o.after_run_off_minor)),
                format!("Users hold {} in their accounts, not counted", sats(o.in_accounts)),
            ],
        );

        for p in &a.providers {
            card(
                &mut els,
                &format!("a_provider_{}", p.id),
                p.name.clone(),
                vec![
                    format!("{:.1} files a day, {:.3} GB-months stored, {:.3} GB sent to people", p.files_per_day, p.gb_months, p.redirected_gb),
                    format!("Earned {} (worth {})", sats(p.earned_sats), opt(p.earned_minor)),
                    format!("Bills recorded: {} · what its prices say: {}", fiat(p.paid_minor.min(i64::MAX as u64) as i64), opt(p.estimated_minor)),
                    format!("Left after the bills: {} · after the estimate: {}", opt(p.margin_vs_paid_minor), opt(p.margin_vs_estimate_minor)),
                ],
            );
        }
        els
    }

    // the books

    fn rate_text(&self) -> String {
        match self.app.store.rate() {
            Ok(Some(r)) => format!(
                "Now: {} for 1 BTC, entered {}",
                self.app.config.fiat.format(r.fiat_minor_per_btc.min(i64::MAX as u64) as i64),
                format_date(r.set_at)
            ),
            _ => "No price entered. Without one the audit uses what you got for the sats you sold.".into(),
        }
    }

    async fn expense(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let provider = text(a, "expense.provider").parse().map_err(|_| bad("Pick a provider."))?;
        let amount = self.app.config.fiat.parse(&text(a, "expense.amount")).ok_or_else(|| bad("The amount isn't a number."))?;
        let date = parse_date(&text(a, "expense.date")).ok_or_else(|| bad("The date should look like 2026-03-31."))?;
        let reference = text(a, "expense.reference");

        let recorded = self.app.record_expense(provider, &reference, amount, date, &text(a, "expense.note")).await?;
        if recorded {
            out.signal(json!({"expense": {"amount": "", "reference": "", "note": ""}}));
            say(out, "success", "Recorded.");
        } else {
            say(out, "info", "That reference was recorded before, nothing changed.");
        }
        self.show_entries(out).await
    }

    async fn sale(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let sold = text(a, "sale.sats").replace([',', ' '], "").parse().map_err(|_| bad("Sats should be a whole number."))?;
        let fiat = self.app.config.fiat.parse(&text(a, "sale.fiat")).ok_or_else(|| bad("The amount received isn't a number."))?;
        let reference = text(a, "sale.reference");

        let recorded = self.app.record_conversion(&reference, sold, fiat, &text(a, "sale.note")).await?;
        if recorded {
            out.signal(json!({"sale": {"sats": "", "fiat": "", "reference": "", "note": ""}}));
            say(out, "success", "Recorded.");
        } else {
            say(out, "info", "That reference was recorded before, nothing changed.");
        }
        self.show_entries(out).await
    }

    fn set_rate(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let rate = self.app.config.fiat.parse(&text(a, "rate.value")).ok_or_else(|| bad("The price isn't a number."))?;
        self.app.set_rate(rate)?;
        out.element(ui::text("rate_now").parent("rate_card").variant("caption").text(self.rate_text()));
        out.signal(json!({"rate": {"value": ""}}));
        say(out, "success", "Saved.");
        Ok(())
    }

    fn set_prices(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let fiat = &self.app.config.fiat;
        let provider = text(a, "prices.provider").parse().map_err(|_| bad("Pick a provider."))?;
        let price = |path: &str| match text(a, path).as_str() {
            "" => Ok(0.0),
            t => fiat.parse_price(t).ok_or_else(|| bad("Prices should be numbers.")),
        };
        self.app.set_prices(
            provider,
            ProviderPrices {
                storage_per_gb_month_minor: price("prices.storage")?,
                egress_per_gb_minor: price("prices.egress")?,
            },
        )?;
        say(out, "success", "Saved.");
        Ok(())
    }

    /// fill the price fields with what is saved for the provider that was picked
    fn load_prices(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let provider: u32 = text(a, "prices.provider").parse().map_err(|_| bad("Pick a provider."))?;
        let fiat = &self.app.config.fiat;
        let (storage, egress) = match self.app.store.prices(provider)? {
            Some(p) => (
                fiat.to_units(p.storage_per_gb_month_minor),
                fiat.to_units(p.egress_per_gb_minor),
            ),
            None => (0.0, 0.0),
        };
        let show = |v: f64| if v == 0.0 { String::new() } else { v.to_string() };
        out.signal(json!({"prices": {"storage": show(storage), "egress": show(egress)}}));
        Ok(())
    }

    async fn show_entries(&mut self, out: &mut Outbox) -> Result<()> {
        let app = self.app.clone();
        let mut entries: Vec<(u64, String)> = vec![];
        for p in &app.config.providers {
            for e in app.ledger.expenses(p.id).await? {
                let label = app.store.note(&crate::ledger::reference_hex_of(e.reference))?.unwrap_or_default();
                entries.push((
                    e.timestamp,
                    format!(
                        "{} · {} paid to {} · {}",
                        format_date(e.date),
                        app.config.fiat.format(e.amount_minor.min(i64::MAX as u64) as i64),
                        provider_name(p),
                        label
                    ),
                ));
            }
        }
        for c in app.ledger.conversions().await? {
            let label = app.store.note(&crate::ledger::reference_hex_of(c.reference))?.unwrap_or_default();
            entries.push((
                c.timestamp,
                format!(
                    "Sold {} for {} · {}",
                    sats(c.sats),
                    app.config.fiat.format(c.fiat_minor.min(i64::MAX as u64) as i64),
                    label
                ),
            ));
        }
        entries.sort_by(|a, b| b.0.cmp(&a.0));

        let mut rows: Vec<Element> = entries
            .into_iter()
            .take(ROWS)
            .enumerate()
            .map(|(i, (_, t))| ui::text(format!("entry_{i}")).parent("entries_card").text(t))
            .collect();
        if rows.is_empty() {
            rows.push(ui::text("entry_none").parent("entries_card").variant("caption").text("Nothing entered yet."));
        }
        replace(out, &mut self.entry_rows, rows);
        Ok(())
    }

    // moderation

    fn file_hash(a: &Action) -> Result<crate::ledger::Hash> {
        parse_hash(&text(a, "mod.hash").to_lowercase()).ok_or_else(|| bad("That isn't a sha256 (64 hex digits)."))
    }

    fn account_key(a: &Action) -> Result<[u8; 32]> {
        parse_pubkey(&text(a, "mod.pubkey")).ok_or_else(|| bad("That isn't a pubkey in hex (64 digits)."))
    }

    fn found(&mut self, out: &mut Outbox, title: String, lines: Vec<String>) {
        let mut els = vec![ui::card("found").parent("mod_found").title(title)];
        if lines.is_empty() {
            els.push(ui::text("found_none").parent("found").variant("caption").text("Nothing."));
        }
        for (i, l) in lines.into_iter().enumerate() {
            els.push(ui::text(format!("found_{i}")).parent("found").variant("code").text(l));
        }
        replace(out, &mut self.found_rows, els);
    }

    async fn uploaders(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let hash = Self::file_hash(a)?;
        let who = self.app.uploaders(&hash).await?;
        let view = self.app.file_view(&hash).await.ok();
        let mut lines = vec![];
        if let Some(v) = &view {
            lines.push(format!(
                "{} bytes, {} left, {}",
                v.size,
                sats(v.balance_sats),
                if v.expired { "expired" } else { "alive" }
            ));
        }
        if self.app.store.is_hash_banned(&lowercase_hex::encode(hash))? {
            lines.push("banned: it can't be uploaded again".into());
        }
        lines.extend(who.into_iter().map(|w| format!("uploaded by {w}")));
        self.found(out, "That file".into(), lines);
        Ok(())
    }

    async fn files_of(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let key = Self::account_key(a)?;
        let files = self.app.ledger.files_by_uploader(&key).await?;
        let lines: Vec<String> = files.iter().map(lowercase_hex::encode).collect();
        let banned = self.app.store.is_pubkey_banned(&lowercase_hex::encode(key))?;
        self.found(
            out,
            format!("{} files by {}{}", lines.len(), short(&lowercase_hex::encode(key)), if banned { " (banned)" } else { "" }),
            lines,
        );
        Ok(())
    }

    fn ask(out: &mut Outbox, text: String, action: &str, arg: String) {
        out.signal(json!({"confirm": {"open": true, "text": text, "action": action, "arg": arg}}));
    }

    fn ask_purge(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let hash = lowercase_hex::encode(Self::file_hash(a)?);
        Self::ask(
            out,
            format!("The file {} will be deleted from its provider, its sats are kept and nobody will be able to upload it again.", short(&hash)),
            "purge",
            hash,
        );
        Ok(())
    }

    fn ask_ban(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let key = lowercase_hex::encode(Self::account_key(a)?);
        let everything = flag(a, "mod.purge");
        Self::ask(
            out,
            format!(
                "The account {} will not be able to pay or upload any more{}.",
                short(&key),
                if everything { ", and everything it uploaded will be deleted for good" } else { "" }
            ),
            if everything { "ban_and_purge" } else { "ban" },
            key,
        );
        Ok(())
    }

    async fn confirmed(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let arg = text(a, "confirm.arg");
        match text(a, "confirm.action").as_str() {
            "purge" => {
                let hash = parse_hash(&arg).ok_or_else(|| bad("That isn't a sha256."))?;
                self.app.purge_file(&hash).await?;
                say(out, "success", "Removed.");
            }
            action @ ("ban" | "ban_and_purge") => {
                let key = parse_pubkey(&arg).ok_or_else(|| bad("That isn't a pubkey."))?;
                let purged = self.app.ban_pubkey(&key, action == "ban_and_purge").await?;
                say(
                    out,
                    "success",
                    if purged.is_empty() { "Banned.".to_string() } else { format!("Banned, and {} files removed.", purged.len()) },
                );
            }
            _ => return Ok(()),
        }
        self.show_bans(out)
    }

    fn show_bans(&mut self, out: &mut Outbox) -> Result<()> {
        let pubkeys = self.app.store.banned_pubkeys()?;
        let hashes = self.app.store.banned_hashes()?;
        let mut rows: Vec<Element> = vec![ui::text("bans_count").parent("bans_card").variant("caption").text(format!(
            "{} accounts and {} files",
            pubkeys.len(),
            hashes.len()
        ))];
        for (i, k) in pubkeys.iter().rev().take(ROWS).enumerate() {
            rows.push(ui::text(format!("ban_key_{i}")).parent("bans_card").variant("code").text(format!("account {k}")));
        }
        for (i, h) in hashes.iter().rev().take(ROWS).enumerate() {
            rows.push(ui::text(format!("ban_hash_{i}")).parent("bans_card").variant("code").text(format!("file {h}")));
        }
        replace(out, &mut self.ban_rows, rows);
        Ok(())
    }

    // rent and orders

    async fn sweep(&mut self, out: &mut Outbox) -> Result<()> {
        let report = self.app.sweep(now() / 86400).await?;
        say(
            out,
            "success",
            format!(
                "{} files were charged {} and {} ran out of sats.",
                report.files,
                sats(report.charged_sats),
                report.expired
            ),
        );
        Ok(())
    }

    fn show_orders(&mut self, out: &mut Outbox) -> Result<()> {
        let since = now().saturating_sub(self.app.config.restrictions.order_retention_hours * 3600);
        let mut orders = self.app.store.waiting_orders(since)?;
        orders.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let mock = matches!(self.app.config.payments, PaymentsConfig::Mock);

        let mut rows = vec![];
        for (i, o) in orders.iter().take(ROWS).enumerate() {
            let what = match &o.kind {
                OrderKind::Account { pubkey } => format!("account {}", short(pubkey)),
                OrderKind::Files { files } => format!("{} files", files.len()),
            };
            let id = format!("order_{i}");
            rows.push(ui::row(&id).parent("orders_card"));
            rows.push(
                ui::text(format!("{id}_text"))
                    .parent(&id)
                    .text(format!("{} · {} · {}", format_date(o.created_at), sats(o.amount_sats), what)),
            );
            if mock {
                rows.push(
                    ui::button(format!("{id}_paid"))
                        .parent(&id)
                        .label("Mark as paid")
                        .on_press(Handler::action("mock_paid").payload(o.id.clone())),
                );
            }
        }
        if rows.is_empty() {
            rows.push(ui::text("order_none").parent("orders_card").variant("caption").text("None."));
        }
        replace(out, &mut self.order_rows, rows);
        Ok(())
    }

    async fn mock_paid(&mut self, out: &mut Outbox, a: &Action) -> Result<()> {
        let id: String = a.payload_as().ok_or_else(|| bad("Which order?"))?;
        self.app.confirm_mock(&id).await?;
        say(out, "success", "Marked as paid.");
        self.show_orders(out)
    }
}
