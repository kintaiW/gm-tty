use std::{future::Future, time::Duration};

use anyhow::Result;

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(60);

/// 断线后指数退避重连，直到程序收到终止信号。
pub async fn run_with_retry<F, Fut>(server_url: &str, token: &str, f: F)
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut delay = INITIAL_DELAY;

    loop {
        match f(server_url.to_owned(), token.to_owned()).await {
            Ok(()) => {
                tracing::info!("session ended normally, reconnecting in {delay:?}");
            }
            Err(e) => {
                tracing::warn!("session error: {e:#}, reconnecting in {delay:?}");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown_signal() => {
                tracing::info!("shutdown signal, exiting");
                return;
            }
        }

        delay = (delay * 2).min(MAX_DELAY);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to listen for ctrl+c");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
