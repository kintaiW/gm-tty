use std::{future::Future, time::Duration};

use anyhow::Result;

const INITIAL_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(30);

/// 断线后重连：
/// - 会话曾成功建立后断开 → 立即重连（delay 归零）
/// - 握手/连接本身失败 → 指数退避（1s → 30s）
pub async fn run_with_retry<F, Fut>(server_url: &str, token: &str, f: F)
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut delay = INITIAL_DELAY;

    loop {
        match f(server_url.to_owned(), token.to_owned()).await {
            Ok(()) => {
                // 会话正常结束（如被 Server 关闭）：立即重连，重置退避
                delay = INITIAL_DELAY;
                tracing::info!("session ended, reconnecting immediately");
            }
            Err(e) => {
                // 连接失败（握手/DNS/TLS）：退避后重试
                tracing::warn!("connect error: {e:#}, retrying in {delay:?}");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = shutdown_signal() => {
                        tracing::info!("shutdown signal, exiting");
                        return;
                    }
                }
                delay = (delay * 2).min(MAX_DELAY);
                continue;
            }
        }

        // 正常断线：仅等待 shutdown 信号，否则立即重连
        tokio::select! {
            biased;
            _ = shutdown_signal() => {
                tracing::info!("shutdown signal, exiting");
                return;
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
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
