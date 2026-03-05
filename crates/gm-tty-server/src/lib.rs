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
use tokio::sync::{Mutex, Notify, RwLock, mpsc};
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

/// Server 向 Agent/Web 发送 WS Ping 的间隔
const PING_INTERVAL: Duration = Duration::from_secs(25);

/// Agent 断线后 session 保留时长
const SESSION_RETAIN: Duration = Duration::from_secs(30);

/// Web 等待 Agent 上线的最长时间
const WAIT_AGENT_TIMEOUT: Duration = Duration::from_secs(30);

// ── 数据结构 ────────────────────────────────────────────────────

struct PendingSession {
    /// Web → Agent 方向（clone-able sender）
    to_agent: mpsc::Sender<Vec<u8>>,
    /// Agent → Web 方向：可替换的 sender。
    /// Agent 的 read task 每次发数据时从这里 clone sender 使用。
    /// 新 Web 连接时替换此 sender（旧 sender 被 drop → 旧 Web 的 receiver 返回 None → 旧 Web 自动退出）。
    web_tx: Arc<Mutex<Option<mpsc::Sender<Vec<u8>>>>>,
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

// ── 静态文件服务 ─────────────────────────────────────────────────
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

    // 注册或复用 session
    let web_tx = {
        let mut sessions = state.sessions.write().await;
        if let Some(s) = sessions.get_mut(&token) {
            // Agent 重连：复用已有 web_tx 和 notify，更新 to_agent
            s.to_agent = to_agent_tx;
            s.agent_online = true;
            let notify = s.agent_notify.clone();
            notify.notify_waiters();
            tracing::info!("agent reconnected, session resumed");
            s.web_tx.clone()
        } else {
            // 全新 session
            let web_tx = Arc::new(Mutex::new(None::<mpsc::Sender<Vec<u8>>>));
            let notify = Arc::new(Notify::new());
            sessions.insert(
                token.clone(),
                PendingSession {
                    to_agent: to_agent_tx,
                    web_tx: web_tx.clone(),
                    agent_online: true,
                    agent_notify: notify,
                },
            );
            web_tx
        }
    };

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Agent → Web：读取 Agent WS 输出，转发到当前 web_tx
    let web_tx_read = web_tx.clone();
    let read_task = async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    let tx = web_tx_read.lock().await.clone();
                    if let Some(tx) = tx {
                        // 如果 Web 端 channel 满了或断开了，忽略（不阻塞 Agent）
                        let _ = tx.try_send(data.to_vec());
                    }
                    // 无 Web 连接时数据丢弃（终端还没被查看）
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
                _ => {}
            }
        }
    };

    // Web → Agent：从 to_agent channel 取数据写给 Agent WS，附带定时 Ping
    let write_task = async {
        let mut ping_ticker = interval(PING_INTERVAL);
        ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ping_ticker.tick().await;

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

    // Agent 断线：标记离线，清理 web_tx，保留 session
    {
        let mut sessions = state.sessions.write().await;
        if let Some(s) = sessions.get_mut(&token) {
            s.agent_online = false;
            // 清掉 web_tx，让当前 Web 的 receiver 自然返回 None → Web 退出
            *s.web_tx.lock().await = None;
        }
    }
    tracing::info!(
        "agent disconnected, retaining session for {}s",
        SESSION_RETAIN.as_secs()
    );

    // 超时清理
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

    // 若 Agent 离线，等待其重连
    let agent_notify = {
        let sessions = state.sessions.read().await;
        match sessions.get(&token) {
            Some(s) if !s.agent_online => Some(s.agent_notify.clone()),
            None => {
                tracing::warn!("no session for token");
                let _ = socket.close().await;
                return;
            }
            _ => None,
        }
    };

    if let Some(notify) = agent_notify {
        tracing::info!(
            "agent offline, waiting up to {}s",
            WAIT_AGENT_TIMEOUT.as_secs()
        );
        match tokio::time::timeout(WAIT_AGENT_TIMEOUT, notify.notified()).await {
            Ok(_) => tracing::info!("agent came online, proceeding"),
            Err(_) => {
                tracing::warn!("timed out waiting for agent");
                let _ = socket.close().await;
                return;
            }
        }
    }

    // 创建此 Web 客户端的专属 channel
    let (new_web_tx, mut web_rx) = mpsc::channel::<Vec<u8>>(256);

    // 获取 to_agent sender，并把新 web_tx 装入 session（踢掉旧 Web）
    let to_agent = {
        let sessions = state.sessions.read().await;
        match sessions.get(&token) {
            Some(session) => {
                let tx = session.to_agent.clone();
                let web_tx_slot = session.web_tx.clone();
                // 替换 web_tx：旧 sender 被 drop → 旧 Web 的 receiver 返回 None → 旧 Web 自动退出
                *web_tx_slot.lock().await = Some(new_web_tx);
                tracing::info!("web bridged (previous web kicked if any)");
                tx
            }
            None => {
                tracing::warn!("session disappeared");
                let _ = socket.close().await;
                return;
            }
        }
    };

    let (mut ws_tx, mut ws_rx) = socket.split();

    // 统一出口 channel：Agent 数据和心跳 Ack 都通过这里发出
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let out_tx_hb = out_tx.clone();

    // Agent 输出 → out channel
    let agent_to_out = async move {
        while let Some(data) = web_rx.recv().await {
            if out_tx.send(data).await.is_err() {
                break;
            }
        }
    };

    // out channel → WebSocket 发送 + 定时 Ping
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
