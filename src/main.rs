mod app;
mod audit;
mod config;
mod http;
mod ledger;
mod operator;
mod payments;
mod storage;
mod store;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::time::Duration;

use app::App;
use config::{Config, LedgerConfig, PaymentsConfig};

async fn build(config: Config) -> Result<Arc<App>, String> {
    let store = Arc::new(store::Store::open(&config.data_dir).map_err(|e| e.to_string())?);

    let ledger: ledger::SharedLedger = match &config.ledger {
        LedgerConfig::Tigerbeetle { cluster_id, addresses } => Arc::new(
            ledger::TigerBeetleLedger::connect(*cluster_id, addresses)
                .await
                .map_err(|e| e.to_string())?,
        ),
        LedgerConfig::Memory => {
            log::warn!("the ledger is in memory, everything will be forgotten when we stop");
            Arc::new(ledger::MemoryLedger::new())
        }
    };

    let payments: payments::SharedPayments = match &config.payments {
        PaymentsConfig::Arkade { server_url, seed, .. } => {
            Arc::new(payments::arkade::Arkade::connect(server_url, seed, store.clone()).await.map_err(|e| e.to_string())?)
        }
        PaymentsConfig::Mock => {
            log::warn!("payments are fake, mark orders as paid in the operator interface");
            Arc::new(payments::MockPayments::default())
        }
    };

    let providers = storage::Providers::from_config(&config.providers).map_err(|e| e.to_string())?;
    Ok(App::new(config, ledger, providers, payments, store))
}

/// look for payments, forever
async fn watch_payments(app: Arc<App>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        app.check_waiting_orders().await;
    }
}

/// once a day take the rent of every file, and keep retrying deletions that failed
async fn collect_rent(app: Arc<App>) {
    const LAST_SWEEP: &str = "last_sweep_day";
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    loop {
        tick.tick().await;

        let day = app::now() / 86400;
        if app.store.meta(LAST_SWEEP).ok().flatten() != Some(day) {
            match app.sweep(day).await {
                Ok(report) => {
                    log::info!("day {day}: charged {} sats to {} files, {} expired", report.charged_sats, report.files, report.expired);
                    if let Err(e) = app.store.set_meta(LAST_SWEEP, day) {
                        log::error!("{e}");
                    }
                }
                Err(e) => log::error!("daily sweep failed, will try again: {e}"),
            }
        }
        app.retry_deletes().await;
        app.save_redirects();
    }
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let path = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("GARDEN_CONFIG").ok())
        .unwrap_or_else(|| "garden.toml".into());
    let config = Config::load(&path).unwrap_or_else(|e| {
        eprintln!("bad config: {e}");
        std::process::exit(1);
    });

    let listen = config.listen.clone();
    let poll = match &config.payments {
        PaymentsConfig::Arkade { poll_secs, .. } => Duration::from_secs((*poll_secs).max(1)),
        PaymentsConfig::Mock => Duration::from_secs(2),
    };
    let app = build(config).await.unwrap_or_else(|e| {
        eprintln!("couldn't start: {e}");
        std::process::exit(1);
    });

    tokio::spawn(watch_payments(app.clone(), poll));
    tokio::spawn(collect_rent(app.clone()));

    let listener = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| {
        eprintln!("couldn't listen on {listen}: {e}");
        std::process::exit(1);
    });
    log::info!("listening on {listen}");
    axum::serve(listener, http::router(app)).await.unwrap();
}
