//! the audit: what the ledger, the wallet and the things the operator typed in say about
//! whether the money adds up and the service pays for itself.
//!
//! everything is optional. without a rate, prices or recorded bills the audit still checks
//! that the ledger is consistent and that the wallet matches it, and says what it couldn't
//! work out.

use serde::Serialize;

use crate::app::{App, Result, now};
use crate::config::{Fiat, Pricing};
use crate::ledger::{self, Expense, FileInfo, Totals};
use crate::store::{DayStats, ProviderPrices, Rate};

const SATS_PER_BTC: i128 = 100_000_000;
const GB: f64 = 1_000_000_000.0;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warning,
    Failure,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub level: Level,
    pub text: String,
}

/// does what came in add up with where it is?
#[derive(Debug, Serialize)]
pub struct Integrity {
    pub received: u64,
    pub in_accounts: u64,
    pub in_files: u64,
    pub earned: u64,
    pub unclaimed: u64,
    /// received - everything above, which is zero unless something is very wrong
    pub difference: i64,
}

/// does the ledger match the wallet?
#[derive(Debug, Serialize)]
pub struct WalletCheck {
    pub actual: u64,
    /// received - sold, plus what is sitting in the addresses of orders still waiting
    pub expected: u64,
    pub waiting_for_orders: u64,
    pub difference: i64,
}

#[derive(Debug, Serialize)]
pub struct Position {
    /// earned and not sold
    pub unsold_sats: i64,
    pub sold_sats: u64,
    /// fiat in the bank: what selling sats brought minus what the bills took. negative means
    /// the operator has been paying out of their own pocket.
    pub fiat_on_hand_minor: i64,
    pub rate_minor_per_btc: Option<u64>,
    /// "operator" if they set it, "realized" if it's the average of what they sold at
    pub rate_source: Option<&'static str>,
    pub rate_set_at: Option<u64>,
    pub unsold_value_minor: Option<i64>,
    /// fiat on hand plus the worth of the unsold sats
    pub net_minor: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Obligations {
    pub open_files: u64,
    pub open_bytes: u64,
    /// sats in the open files, which is what they have left to pay in rent
    pub in_open_files: u64,
    /// sats users hold in their accounts and haven't spent on files
    pub in_accounts: u64,
    pub files_without_price: u64,
    /// what hosting the open files until their sats run out will cost at today's prices
    pub projected_cost_minor: Option<i64>,
    /// what those sats are worth, because that's what the files will pay
    pub future_rent_minor: Option<i64>,
    /// net position + future rent - projected cost
    pub after_run_off_minor: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ProviderAudit {
    pub id: u32,
    pub name: String,
    pub files_per_day: f64,
    pub gb_months: f64,
    pub redirected_gb: f64,
    pub earned_sats: u64,
    pub earned_minor: Option<i64>,
    /// what the operator recorded paying for bills dated in the period
    pub paid_minor: u64,
    /// what the prices the operator entered say it should have cost
    pub estimated_minor: Option<i64>,
    pub margin_vs_paid_minor: Option<i64>,
    pub margin_vs_estimate_minor: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Audit {
    pub generated_at: u64,
    pub from_day: u64,
    pub to_day: u64,
    pub integrity: Integrity,
    pub wallet: Option<WalletCheck>,
    pub position: Position,
    pub obligations: Obligations,
    pub providers: Vec<ProviderAudit>,
    pub warnings: Vec<Warning>,
}

pub struct ProviderInfo {
    pub id: u32,
    pub name: String,
    pub prices: Option<ProviderPrices>,
}

/// everything the audit looks at
pub struct Inputs {
    pub now: u64,
    pub from_day: u64,
    pub to_day: u64,
    pub fiat: Fiat,
    pub pricing: Pricing,
    pub totals: Totals,
    pub in_accounts: u64,
    /// every file account, closed ones too
    pub files: Vec<FileInfo>,
    pub wallet: Option<u64>,
    pub waiting_for_orders: u64,
    pub rate: Option<Rate>,
    pub providers: Vec<ProviderInfo>,
    pub stats: Vec<(u64, u32, DayStats)>,
    pub expenses: Vec<Expense>,
}

fn value(sats: i128, rate: u64) -> i64 {
    (sats * rate as i128 / SATS_PER_BTC).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn clamp(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

pub fn compute(i: Inputs) -> Audit {
    let mut warnings: Vec<Warning> = vec![];
    let mut warn = |level, text: String| warnings.push(Warning { level, text });
    let fiat = |minor: i64| i.fiat.format(minor);
    let t = i.totals;

    // the ledger against itself
    let in_files: u64 = i.files.iter().map(|f| f.balance).sum();
    let difference = clamp(t.received as i128 - (i.in_accounts + in_files + t.earned + t.unclaimed) as i128);
    if difference != 0 {
        warn(
            Level::Failure,
            format!("The ledger doesn't add up: {difference} sats are missing or invented. Stop and look at the transfers."),
        );
    }
    if t.unclaimed > 0 {
        warn(
            Level::Info,
            format!("{} sats were paid and not claimed by any order (overpayments, or files that expired before their payment arrived). They are yours to keep or return.", t.unclaimed),
        );
    }

    // the ledger against the wallet
    let wallet = i.wallet.map(|actual| {
        let expected = (t.received as i128 - t.sold as i128 + i.waiting_for_orders as i128).max(0) as u64;
        let difference = clamp(actual as i128 - expected as i128);
        match difference {
            0 => {}
            d if d > 0 => warn(
                Level::Warning,
                format!("The wallet holds {d} sats more than the books explain: money arrived that no order accounts for (a payment to an address after its order was settled, or less than the order asked)."),
            ),
            d => warn(
                Level::Failure,
                format!("The wallet holds {} sats less than the books say. Either sats left it that weren't recorded as sold, or they are gone (fees count).", -d),
            ),
        }
        WalletCheck {
            actual,
            expected,
            waiting_for_orders: i.waiting_for_orders,
            difference,
        }
    });

    // what the operator has
    let unsold = t.earned as i128 - t.sold as i128;
    if unsold < 0 {
        warn(
            Level::Failure,
            format!("{} more sats were recorded as sold than rent has ever earned.", -unsold),
        );
    }
    let fiat_on_hand = clamp(t.fiat_in as i128 - t.fiat_out as i128);
    if fiat_on_hand < 0 {
        warn(
            Level::Info,
            format!("Bills paid add up to {} more than the sats sold brought in: the difference came from elsewhere.", fiat(-fiat_on_hand)),
        );
    }
    let realized = (t.sold > 0).then(|| (t.fiat_in as i128 * SATS_PER_BTC / t.sold as i128) as u64);
    let (rate, rate_source, rate_set_at) = match (i.rate, realized) {
        (Some(r), _) => (Some(r.fiat_minor_per_btc), Some("operator"), Some(r.set_at)),
        (None, Some(r)) => (Some(r), Some("realized"), None),
        (None, None) => (None, None, None),
    };
    match (i.rate, rate) {
        (None, None) => warn(Level::Info, "Set the price of bitcoin (or record a sale) to see what your sats are worth.".into()),
        (Some(r), _) if i.now.saturating_sub(r.set_at) > 7 * 86400 => {
            warn(Level::Info, "The price of bitcoin you entered is more than a week old.".into())
        }
        _ => {}
    }
    let unsold_value = rate.map(|r| value(unsold, r));
    let net = unsold_value.map(|v| fiat_on_hand.saturating_add(v));
    let position = Position {
        unsold_sats: clamp(unsold),
        sold_sats: t.sold,
        fiat_on_hand_minor: fiat_on_hand,
        rate_minor_per_btc: rate,
        rate_source,
        rate_set_at,
        unsold_value_minor: unsold_value,
        net_minor: net,
    };

    // what it will cost to keep the promises
    let price_of = |provider: u32| {
        i.providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|p| p.prices)
            .filter(|p| p.storage_per_gb_month_minor > 0.0)
    };
    let (mut open_files, mut open_bytes, mut in_open_files) = (0u64, 0u64, 0u64);
    let (mut projected, mut unpriced, mut priced) = (0f64, 0u64, 0u64);
    for f in i.files.iter().filter(|f| !f.closed) {
        open_files += 1;
        open_bytes += f.size;
        in_open_files += f.balance;
        match price_of(f.provider) {
            Some(p) => {
                priced += 1;
                let days_left = f.balance as f64 / i.pricing.rent_per_day(f.size) as f64;
                projected += days_left * (f.size as f64 / GB) * p.storage_per_gb_month_minor / 30.0;
            }
            None => unpriced += 1,
        }
    }
    let projected_cost = (priced > 0).then(|| projected.round() as i64);
    let future_rent = rate.map(|r| value(in_open_files as i128, r));
    let after_run_off = match (net, future_rent, projected_cost) {
        (Some(n), Some(f), Some(c)) => Some(n.saturating_add(f).saturating_sub(c)),
        _ => None,
    };
    if unpriced > 0 && open_files > 0 {
        warn(
            Level::Info,
            format!("{unpriced} open files are on providers with no storage price, so they aren't counted in the projected cost."),
        );
    }
    if let Some(a) = after_run_off
        && a < 0
    {
        warn(
            Level::Warning,
            format!("If every open file stayed until its sats ran out, hosting would cost {} more than you hold and the files will still pay. Prices may be too low.", fiat(-a)),
        );
    }
    let obligations = Obligations {
        open_files,
        open_bytes,
        in_open_files,
        in_accounts: i.in_accounts,
        files_without_price: unpriced,
        projected_cost_minor: projected_cost,
        future_rent_minor: future_rent,
        after_run_off_minor: after_run_off,
    };

    // each provider in the period
    let days = (i.to_day + 1 - i.from_day).max(1);
    let (since, until) = (i.from_day * 86400, (i.to_day + 1) * 86400);
    let mut providers = vec![];
    if i.stats.is_empty() {
        warn(Level::Info, "No daily charges were recorded in this period, so the figures per provider are empty.".into());
    }
    for p in &i.providers {
        let rows = i.stats.iter().filter(|(_, id, _)| *id == p.id).map(|(_, _, s)| s);
        let (mut files, mut bytes, mut earned, mut redirected) = (0u64, 0u64, 0u64, 0u64);
        for s in rows {
            files += s.files;
            bytes += s.bytes;
            earned += s.rent_sats;
            redirected += s.redirected_bytes;
        }
        let gb_months = bytes as f64 / GB / 30.0;
        let redirected_gb = redirected as f64 / GB;
        let paid: u64 = i
            .expenses
            .iter()
            .filter(|e| e.provider == p.id && (since..until).contains(&e.date))
            .map(|e| e.amount_minor)
            .sum();
        let estimated = p.prices.map(|pr| {
            (gb_months * pr.storage_per_gb_month_minor + redirected_gb * pr.egress_per_gb_minor).round() as i64
        });
        let earned_minor = rate.map(|r| value(earned as i128, r));
        let margin_vs_paid = earned_minor.map(|e| e.saturating_sub(clamp(paid as i128)));
        let margin_vs_estimate = earned_minor.zip(estimated).map(|(e, c)| e.saturating_sub(c));

        if let Some(est) = estimated
            && paid > 0
            && est > 0
            && paid as f64 > est as f64 * 1.25
        {
            warn(
                Level::Warning,
                format!(
                    "{}: you paid {} and its prices with your files add up to {} ({:.0}% more). Something besides stored bytes and redirects is being charged, or the prices are out of date.",
                    p.name,
                    fiat(clamp(paid as i128)),
                    fiat(est),
                    (paid as f64 / est as f64 - 1.0) * 100.0
                ),
            );
        }
        if let Some(m) = margin_vs_paid.filter(|_| paid > 0)
            && m < 0
        {
            warn(
                Level::Warning,
                format!("{}: it cost {} more than it earned in this period.", p.name, fiat(-m)),
            );
        } else if let Some(m) = margin_vs_estimate
            && m < 0
            && paid == 0
        {
            warn(
                Level::Warning,
                format!("{}: at its prices it costs {} more than it earns.", p.name, fiat(-m)),
            );
        }
        if paid == 0 && estimated.is_some_and(|e| e > 0) && days >= 30 {
            warn(Level::Info, format!("{}: no bill dated in this period has been recorded.", p.name));
        }

        providers.push(ProviderAudit {
            id: p.id,
            name: p.name.clone(),
            files_per_day: files as f64 / days as f64,
            gb_months,
            redirected_gb,
            earned_sats: earned,
            earned_minor,
            paid_minor: paid,
            estimated_minor: estimated,
            margin_vs_paid_minor: margin_vs_paid,
            margin_vs_estimate_minor: margin_vs_estimate,
        });
    }

    Audit {
        generated_at: i.now,
        from_day: i.from_day,
        to_day: i.to_day,
        integrity: Integrity {
            received: t.received,
            in_accounts: i.in_accounts,
            in_files,
            earned: t.earned,
            unclaimed: t.unclaimed,
            difference,
        },
        wallet,
        position,
        obligations,
        providers,
        warnings,
    }
}

impl App {
    /// run the audit over the last `days` days
    pub async fn audit(&self, days: u64) -> Result<Audit> {
        self.save_redirects();

        let to_day = now() / 86400;
        let from_day = to_day.saturating_sub(days.max(1) - 1);

        let mut in_accounts = 0;
        ledger::all_users(&*self.ledger, |u| in_accounts += u.balance).await?;
        let mut files = vec![];
        ledger::all_files(&*self.ledger, |f| files.push(f)).await?;

        let wallet = match self.payments.wallet_balance().await {
            Ok(w) => w,
            Err(e) => {
                log::warn!("couldn't read the wallet for the audit: {e}");
                None
            }
        };
        let mut waiting_for_orders = 0;
        if wallet.is_some() {
            let since = now().saturating_sub(self.config.restrictions.order_retention_hours * 3600);
            for order in self.store.waiting_orders(since)? {
                waiting_for_orders += self.payments.received(&order.payment).await.unwrap_or(0);
            }
        }

        let mut providers = vec![];
        let mut expenses = vec![];
        for p in &self.config.providers {
            providers.push(ProviderInfo {
                id: p.id,
                name: format!("{} ({:?})", p.id, p.kind).to_lowercase(),
                prices: self.store.prices(p.id)?,
            });
            expenses.extend(self.ledger.expenses(p.id).await?);
        }

        Ok(compute(Inputs {
            now: now(),
            from_day,
            to_day,
            fiat: self.config.fiat.clone(),
            pricing: self.config.pricing.clone(),
            totals: self.ledger.totals().await?,
            in_accounts,
            files,
            wallet,
            waiting_for_orders,
            rate: self.store.rate()?,
            providers,
            stats: self.store.day_stats(from_day, to_day)?,
            expenses,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 20_000;

    fn file(size: u64, balance: u64, provider: u32, closed: bool) -> FileInfo {
        FileInfo {
            hash: [1; 32],
            size,
            provider,
            balance,
            closed,
            timestamp: 1,
        }
    }

    /// 1000 sats received, 400 in accounts, 100 in a file, 450 earned of which 100 sold for
    /// 5000 minor units, 50 unclaimed. 2 bills were paid.
    fn inputs() -> Inputs {
        Inputs {
            now: DAY * 86400 + 5,
            from_day: DAY - 29,
            to_day: DAY,
            fiat: Fiat::default(),
            pricing: Pricing::default(),
            totals: Totals {
                received: 1000,
                earned: 450,
                sold: 100,
                unclaimed: 50,
                fiat_in: 5000,
                fiat_out: 700,
            },
            in_accounts: 400,
            files: vec![file(1_000_000_000, 100, 1, false), file(10, 0, 1, true)],
            wallet: Some(900),
            waiting_for_orders: 0,
            rate: None,
            providers: vec![
                ProviderInfo { id: 1, name: "1 (b2)".into(), prices: None },
                ProviderInfo { id: 2, name: "2 (s3)".into(), prices: None },
            ],
            stats: vec![
                (DAY - 1, 1, DayStats { files: 2, bytes: 3_000_000_000, rent_sats: 200, redirected_bytes: 0 }),
                (DAY, 1, DayStats { files: 2, bytes: 3_000_000_000, rent_sats: 250, redirected_bytes: 2_000_000_000 }),
            ],
            expenses: vec![
                Expense { provider: 1, amount_minor: 500, date: (DAY - 3) * 86400, timestamp: 1, reference: 1 },
                Expense { provider: 1, amount_minor: 200, date: (DAY - 100) * 86400, timestamp: 2, reference: 2 },
            ],
        }
    }

    fn has(a: &Audit, level: Level, containing: &str) -> bool {
        a.warnings.iter().any(|w| w.level == level && w.text.contains(containing))
    }

    #[test]
    fn a_consistent_book_with_nothing_entered() {
        let a = compute(inputs());
        assert_eq!(a.integrity.difference, 0);
        let w = a.wallet.as_ref().unwrap();
        assert_eq!((w.expected, w.difference), (900, 0));
        assert_eq!(a.position.unsold_sats, 350);
        assert_eq!(a.position.fiat_on_hand_minor, 4300);
        // we know what sats fetched when sold: 5000 for 100 sats
        assert_eq!(a.position.rate_source, Some("realized"));
        assert_eq!(a.position.rate_minor_per_btc, Some(5_000_000_000));
        // only what's in the period and in the provider's bills counts
        let p = &a.providers[0];
        assert_eq!((p.earned_sats, p.paid_minor), (450, 500));
        assert!((p.files_per_day - 4.0 / 30.0).abs() < 1e-9);
        assert_eq!(p.estimated_minor, None);
        assert_eq!(a.providers[1].earned_sats, 0);
        assert!(has(&a, Level::Info, "50 sats were paid and not claimed"));
        assert!(!a.warnings.iter().any(|w| w.level != Level::Info), "{:?}", a.warnings);
    }

    #[test]
    fn the_ledger_must_add_up() {
        let mut i = inputs();
        i.totals.received += 7;
        let a = compute(i);
        assert_eq!(a.integrity.difference, 7);
        assert!(has(&a, Level::Failure, "doesn't add up"));
    }

    #[test]
    fn the_wallet_must_match() {
        let mut i = inputs();
        i.wallet = Some(940);
        let a = compute(i);
        assert_eq!(a.wallet.as_ref().unwrap().difference, 40);
        assert!(has(&a, Level::Warning, "40 sats more"));

        // sats that sit at the address of an order that is still open are expected
        let mut i = inputs();
        i.wallet = Some(940);
        i.waiting_for_orders = 40;
        let a = compute(i);
        assert_eq!(a.wallet.as_ref().unwrap().difference, 0);
        assert!(a.warnings.iter().all(|w| w.level == Level::Info));

        let mut i = inputs();
        i.wallet = Some(850);
        let a = compute(i);
        assert!(has(&a, Level::Failure, "50 sats less"));

        let mut i = inputs();
        i.wallet = None;
        assert!(compute(i).wallet.is_none());
    }

    #[test]
    fn selling_more_than_earned_is_flagged() {
        let mut i = inputs();
        i.totals.sold = 500;
        assert!(has(&compute(i), Level::Failure, "50 more sats were recorded as sold"));
    }

    #[test]
    fn an_entered_rate_wins_and_values_everything() {
        let mut i = inputs();
        // 1 BTC = 10,000.00
        i.rate = Some(Rate { fiat_minor_per_btc: 1_000_000, set_at: i.now });
        let a = compute(i);
        assert_eq!(a.position.rate_source, Some("operator"));
        // 350 sats are 0.035
        assert_eq!(a.position.unsold_value_minor, Some(3));
        assert_eq!(a.position.net_minor, Some(4303));
        assert_eq!(a.providers[0].earned_minor, Some(4));
        assert_eq!(a.providers[0].margin_vs_paid_minor, Some(4 - 500));
        assert!(has(&a, Level::Warning, "it cost 4.96 USD more than it earned"));
    }

    #[test]
    fn old_rates_are_pointed_out() {
        let mut i = inputs();
        i.rate = Some(Rate { fiat_minor_per_btc: 1_000_000, set_at: i.now - 8 * 86400 });
        assert!(has(&compute(i), Level::Info, "more than a week old"));
    }

    #[test]
    fn what_was_paid_is_compared_with_what_the_prices_say() {
        let mut i = inputs();
        // 0.6 cents per GB-month, 1 cent per GB of egress.
        // stored: 6 GB-days = 0.2 GB-months; redirected 2 GB
        i.providers[0].prices = Some(ProviderPrices { storage_per_gb_month_minor: 0.6, egress_per_gb_minor: 1.0 });
        let a = compute(i);
        let p = &a.providers[0];
        assert!((p.gb_months - 0.2).abs() < 1e-9);
        assert_eq!(p.estimated_minor, Some(2));
        // paid 5.00 for something that should cost 0.02
        assert!(has(&a, Level::Warning, "you paid 5.00 USD and its prices with your files add up to 0.02 USD"));
        assert!(has(&a, Level::Warning, "more). Something besides"));

        // close enough is not flagged: 0.2 GB-months * 100 + 2 GB * 240 = 500
        let mut i = inputs();
        i.providers[0].prices = Some(ProviderPrices { storage_per_gb_month_minor: 100.0, egress_per_gb_minor: 240.0 });
        let a = compute(i);
        assert_eq!(a.providers[0].estimated_minor, Some(500));
        assert!(!has(&a, Level::Warning, "Something besides"));
    }

    #[test]
    fn running_the_open_files_until_they_die() {
        let mut i = inputs();
        i.rate = Some(Rate { fiat_minor_per_btc: 1_000_000_000_000, set_at: i.now });
        // a 1 GB file with 100 sats; rent is 1 + 96 sats/day, so it lasts about a day
        i.providers[0].prices = Some(ProviderPrices { storage_per_gb_month_minor: 300.0, egress_per_gb_minor: 0.0 });
        let a = compute(i);
        let o = &a.obligations;
        assert_eq!((o.open_files, o.open_bytes, o.in_open_files), (1, 1_000_000_000, 100));
        // 100/97 days at 300 per GB-month, which is 10 a day = about 10
        assert_eq!(o.projected_cost_minor, Some(10));
        // 100 sats at 10,000,000,000.00 per BTC are 1,000,000 minor units: it more than pays
        assert_eq!(o.future_rent_minor, Some(1_000_000));
        assert!(o.after_run_off_minor.unwrap() > 0);

        // but if hosting is expensive, it doesn't
        let mut i = inputs();
        i.rate = Some(Rate { fiat_minor_per_btc: 1_000, set_at: i.now });
        i.providers[0].prices = Some(ProviderPrices { storage_per_gb_month_minor: 3_000_000_000.0, egress_per_gb_minor: 0.0 });
        i.totals.fiat_in = 0;
        i.totals.fiat_out = 0;
        let a = compute(i);
        assert!(a.obligations.after_run_off_minor.unwrap() < 0);
        assert!(has(&a, Level::Warning, "Prices may be too low"));
    }

    #[test]
    fn files_on_unpriced_providers_are_said_to_be_left_out() {
        let a = compute(inputs());
        assert_eq!(a.obligations.files_without_price, 1);
        assert_eq!(a.obligations.projected_cost_minor, None);
        assert!(has(&a, Level::Info, "1 open files are on providers with no storage price"));
    }
}
