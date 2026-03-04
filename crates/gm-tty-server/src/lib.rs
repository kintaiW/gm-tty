use std::{collections::HashMap, sync::Arc};

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
use tokio::sync::{RwLock, mpsc};
use tower_http::trace::TraceLayer;
use tracing::instrument;

mod auth;
mod config;

pub use config::Config;

// ── 静态资源内嵌 ────────────────────────────────────────────────
#[derive(Embed)]
#[folder = "../../frontend/"]
struct FrontendAssets;

// ── 数据结构 ────────────────────────────────────────────────────

/// Agent 注册后创建的半连接会话
struct PendingSession {
    /// Server 发给 Agent 的通道 (Web→Agent 方向)
    to_agent: mpsc::Sender<Vec<u8>>,
    /// Agent 发来数据的接收端 (Agent→Web 方向), 给 Web 连接时取走
    from_agent: Option<mpsc::Receiver<Vec<u8>>>,
}

type Sessions = Arc<RwLock<HashMap<String, PendingSession>>>;

#[derive(Clone)]
struct AppState {
    sessions: Sessions,
    /// 允许连接的 token 集合（MVP 阶段从配置加载）
    valid_tokens: Arc<Vec<String>>,
}

// ── 公共 Token 生成工具 ─────────────────────────────────────────
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

// ── Server 入口 ─────────────────────────────────────────────────
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

// ── 静态文件服务 ────────────────────────────────────────────────
async fn serve_index() -> impl IntoResponse {
    serve_file("index.html")
}

async fn serve_terminal(Path(token): Path<String>) -> impl IntoResponse {
    // 为 token 路径提供同一个 index.html，JS 从 URL 读取 token
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

// ── Agent WebSocket 处理 ────────────────────────────────────────
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

    // 注册会话
    {
        let mut sessions = state.sessions.write().await;
        sessions.insert(
            token.clone(),
            PendingSession {
                to_agent: to_agent_tx,
                from_agent: Some(from_agent_rx),
            },
        );
    }

    let (mut ws_tx, mut ws_rx) = socket.split();

    // Agent → Server (读取 Agent 输出，放入 from_agent channel)
    let read_task = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    if from_agent_tx.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };

    // Server → Agent (从 to_agent channel 取数据写给 Agent)
    let write_task = async {
        while let Some(data) = to_agent_rx.recv().await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = read_task => {},
        _ = write_task => {},
    }

    // 清理
    state.sessions.write().await.remove(&token);
    tracing::info!("agent disconnected");
}

// ── Web WebSocket 处理 ──────────────────────────────────────────
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
                tracing::warn!("no agent for token, closing web ws");
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

    // out channel → WebSocket 发送
    let out_to_ws = async move {
        while let Some(data) = out_rx.recv().await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
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
