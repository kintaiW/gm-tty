use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    Router,
    extract::{
        OriginalUri, Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use gm_tty_protocol::{Frame, FrameType};
use rand::RngCore;
use rust_embed::Embed;
use tokio::sync::{Notify, RwLock, mpsc};
use tokio::time::{MissedTickBehavior, interval};
use tower_http::trace::TraceLayer;
use tracing::instrument;

mod auth;
mod config;

pub use config::Config;

// ── 静态资源内嵌 ────────────────────────────────────────────────
#[derive(Embed)]
#[folder = "../../frontend/"]
struct FrontendAssets;

// ── 常量 ────────────────────────────────────────────────────────

/// Server 向 Agent/Web 发送 WS Ping 的间隔，防止反向代理空闲超时
const PING_INTERVAL: Duration = Duration::from_secs(25);

/// Agent 断线后 session 保留时长，给 Agent 重连的窗口
const SESSION_RETAIN: Duration = Duration::from_secs(30);

/// Web 等待 Agent 上线的最长时间
const WAIT_AGENT_TIMEOUT: Duration = Duration::from_secs(30);

// ── 数据结构 ────────────────────────────────────────────────────

struct PendingSession {
    /// Server 发给 Agent 的通道（Web→Agent 方向）
    to_agent: mpsc::Sender<Vec<u8>>,
    /// Agent 发来数据的接收端（Agent→Web 方向），Web 连接时取走
    from_agent: Option<mpsc::Receiver<Vec<u8>>>,
    /// Agent 是否当前在线
    agent_online: bool,
    /// Agent 重新上线时通知等待中的 Web
    agent_notify: Arc<Notify>,
}

type Sessions = Arc<RwLock<HashMap<String, PendingSession>>>;

#[derive(Clone)]
struct AppState {
    sessions: Sessions,
    valid_tokens: Arc<Vec<String>>,
}

// ── Token 生成 ───────────────────────────────────────────────────
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

// ── Server 入口 ──────────────────────────────────────────────────
pub async fn run(config: Config) -> Result<()> {
    let state = AppState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        valid_tokens: Arc::new(config.tokens.clone()),
    };

    let app = Router::new()
        .route("/ws/agent/{token}", get(handle_agent_ws))
        .route("/ws/web/{token}", get(handle_web_ws))
        .route("/terminal/{token}", get(serve_terminal))
        .route("/", get(serve_index))
        .fallback(get(serve_static))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: std::net::SocketAddr = config.listen_addr.parse()?;
    tracing::info!("server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ── 静态文件服务 ──────────────────────────────────────────���──────
async fn serve_index() -> impl IntoResponse {
    serve_file("index.html")
}

async fn serve_terminal(Path(token): Path<String>) -> impl IntoResponse {
    let _ = token;
    serve_file("index.html")
}

async fn serve_static(OriginalUri(uri): OriginalUri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    serve_file(path)
}

fn serve_file(path: &str) -> impl IntoResponse {
    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime)],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ── Agent WebSocket 处理 ─────────────────────────────────────────
#[instrument(skip_all, fields(token = %token))]
async fn handle_agent_ws(
    ws: WebSocketUpgrade,
    Path(token): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if !state.valid_tokens.contains(&token) {
        tracing::warn!("agent: invalid token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| agent_connected(socket, token, state))
        .into_response()
}

async fn agent_connected(socket: WebSocket, token: String, state: AppState) {
    tracing::info!("agent connected");

    let (to_agent_tx, mut to_agent_rx) = mpsc::channel::<Vec<u8>>(256);
    let (from_agent_tx, from_agent_rx) = mpsc::channel::<Vec<u8>>(256);

    // 注册或复用 session
    let agent_notify = {
        let mut sessions = state.sessions.write().await;
        if let Some(s) = sessions.get_mut(&token) {
            // Agent 重连：复用已有 notify，重建 channels，唤醒等待中的 Web
            s.to_agent = to_agent_tx;
            s.from_agent = Some(from_agent_rx);
            s.agent_online = true;
            let notify = s.agent_notify.clone();
            notify.notify_waiters();
            tracing::info!("agent reconnected, session resumed");
            notify
        } else {
            // 全新 session
            let notify = Arc::new(Notify::new());
            sessions.insert(
                token.clone(),
                PendingSession {
                    to_agent: to_agent_tx,
                    from_agent: Some(from_agent_rx),
                    agent_online: true,
                    agent_notify: notify.clone(),
                },
            );
            notify
        }
    };
    let _ = agent_notify; // 保持 Arc 引用避免 notify 被提前释放

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Agent → Server：读取 Agent 输出放入 from_agent channel
    let read_task = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    if from_agent_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {}
            }
        }
    };

    // Server → Agent：从 to_agent channel 取数据写给 Agent，附带定时 Ping
    let write_task = async {
        let mut ping_ticker = interval(PING_INTERVAL);
        ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ping_ticker.tick().await; // 跳过第一个立即触发

        loop {
            tokio::select! {
                data = to_agent_rx.recv() => {
                    match data {
                        Some(bytes) => {
                            if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_ticker.tick() => {
                    tracing::trace!("ping agent");
                    if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = read_task => {},
        _ = write_task => {},
    }

    // Agent 断线：标记离线，保留 session，启动清理定时器
    {
        let mut sessions = state.sessions.write().await;
        if let Some(s) = sessions.get_mut(&token) {
            s.agent_online = false;
            // 取出 from_agent，防止 Web 继续从已关闭的 channel 读
            s.from_agent = None;
        }
    }
    tracing::info!("agent disconnected, retaining session for {}s", SESSION_RETAIN.as_secs());

    // 超时后若 Agent 未重连则清理 session
    let state_clone = state.clone();
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(SESSION_RETAIN).await;
        let mut sessions = state_clone.sessions.write().await;
        if let Some(s) = sessions.get(&token_clone) {
            if !s.agent_online {
                sessions.remove(&token_clone);
                tracing::info!("session expired: token={token_clone}");
            }
        }
    });
}

// ── Web WebSocket 处理 ───────────────────────────────────────────
#[instrument(skip_all, fields(token = %token))]
async fn handle_web_ws(
    ws: WebSocketUpgrade,
    Path(token): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    if !state.valid_tokens.contains(&token) {
        tracing::warn!("web: invalid token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.on_upgrade(move |socket| web_connected(socket, token, state))
        .into_response()
}

async fn web_connected(mut socket: WebSocket, token: String, state: AppState) {
    tracing::info!("web connected");

    // 若 Agent 离线，等待其重连（最多 WAIT_AGENT_TIMEOUT）
    let agent_notify = {
        let sessions = state.sessions.read().await;
        match sessions.get(&token) {
            Some(s) if !s.agent_online => Some(s.agent_notify.clone()),
            None => {
                // session 完全不存在（token 未使用或已过期）
                tracing::warn!("no session for token");
                let _ = socket.close().await;
                return;
            }
            _ => None, // agent_online == true，直接继续
        }
    };

    if let Some(notify) = agent_notify {
        tracing::info!("agent offline, waiting up to {}s", WAIT_AGENT_TIMEOUT.as_secs());
        match tokio::time::timeout(WAIT_AGENT_TIMEOUT, notify.notified()).await {
            Ok(_) => tracing::info!("agent came online, proceeding"),
            Err(_) => {
                tracing::warn!("timed out waiting for agent");
                let _ = socket.close().await;
                return;
            }
        }
    }

    // 取出 Agent 的数据通道
    let (to_agent, from_agent) = {
        let mut sessions = state.sessions.write().await;
        match sessions.get_mut(&token) {
            Some(session) => {
                let tx = session.to_agent.clone();
                let rx = session.from_agent.take();
                (tx, rx)
            }
            None => {
                tracing::warn!("session disappeared before web could connect");
                let _ = socket.close().await;
                return;
            }
        }
    };

    let Some(mut from_agent) = from_agent else {
        tracing::warn!("from_agent already consumed (duplicate web connection)");
        let _ = socket.close().await;
        return;
    };

    let (mut ws_tx, mut ws_rx) = socket.split();

    // 统一出口 channel：Agent 数据和心跳 Ack 都通过这里发出
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let out_tx_hb = out_tx.clone();

    // Agent 输出 → out channel
    let agent_to_out = async move {
        while let Some(data) = from_agent.recv().await {
            if out_tx.send(data).await.is_err() {
                break;
            }
        }
    };

    // out channel → WebSocket 发送，附带定时 Ping
    let out_to_ws = async move {
        let mut ping_ticker = interval(PING_INTERVAL);
        ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ping_ticker.tick().await;

        loop {
            tokio::select! {
                data = out_rx.recv() => {
                    match data {
                        Some(bytes) => {
                            if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_ticker.tick() => {
                    tracing::trace!("ping web");
                    if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    // Web 输入 → Agent（心跳 Ack 走 out channel）
    let forward_to_agent = async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    if let Ok(frame) = Frame::decode(&data) {
                        match frame.frame_type {
                            FrameType::Heartbeat => {
                                let ack = Frame::heartbeat_ack().encode();
                                let _ = out_tx_hb.send(ack).await;
                            }
                            FrameType::Close => break,
                            _ => {
                                if to_agent.send(data.to_vec()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {}
            }
        }
    };

    tokio::select! {
        _ = agent_to_out => {},
        _ = out_to_ws => {},
        _ = forward_to_agent => {},
    }

    tracing::info!("web disconnected");
}
