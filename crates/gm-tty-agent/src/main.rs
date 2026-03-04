use anyhow::Result;
use std::io::{Read, Write};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use gm_tty_protocol::{Frame, FrameType};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::{connect_async, tungstenite::Message};

mod reconnect;

/// 每 25s 发一次 WS Ping，防止反向代理空闲超时（通常为 60s）
const PING_INTERVAL: Duration = Duration::from_secs(25);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("gm_tty_agent=info".parse()?),
        )
        .init();

    let server_url = std::env::var("GM_TTY_SERVER")
        .unwrap_or_else(|_| "ws://127.0.0.1:7093".to_string());
    let token = std::env::var("GM_TTY_TOKEN").expect("GM_TTY_TOKEN must be set");

    reconnect::run_with_retry(&server_url, &token, run_session).await;
    Ok(())
}

async fn run_session(server_url: String, token: String) -> Result<()> {
    let url = format!("{server_url}/ws/agent/{token}");
    tracing::info!("connecting to {url}");

    let (ws_stream, _) = connect_async(&url).await?;
    tracing::info!("connected to server");

    // 启动 PTY (初始 80×24)
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let shell = default_shell();
    tracing::info!("starting shell: {shell}");

    let cmd = CommandBuilder::new(&shell);
    let _child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    let mut pty_reader = pair.master.try_clone_reader()?;
    let mut pty_writer = pair.master.take_writer()?;

    let (ws_tx, ws_rx) = ws_stream.split();

    // channel: PTY stdout → WebSocket sender
    let (pty_out_tx, pty_out_rx) = mpsc::channel::<Vec<u8>>(256);

    // 阻塞线程读取 PTY 输出
    let pty_read_handle = tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        loop {
            match pty_reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let frame = Frame::new_data(&buf[..n]).encode();
                    if pty_out_tx.blocking_send(frame).is_err() {
                        break;
                    }
                }
            }
        }
        tracing::info!("pty reader exited");
    });

    // WebSocket 发送任务：PTY 输出 + 定时 Ping 保活
    let send_task = async {
        let mut ws_tx = ws_tx;
        let mut pty_out_rx = pty_out_rx;
        let mut ping_ticker = interval(PING_INTERVAL);
        ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // 第一个 tick 立即触发，跳过它
        ping_ticker.tick().await;

        loop {
            tokio::select! {
                data = pty_out_rx.recv() => {
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
                    tracing::trace!("sending ws ping");
                    if ws_tx.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    // WebSocket 接收任务：把 Server 数据写入 PTY
    let recv_task = async {
        let master = &pair.master;
        let mut ws_rx = ws_rx;

        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    match Frame::decode(&data) {
                        Ok(frame) => {
                            if !handle_frame(frame, &mut pty_writer, master) {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("frame decode error: {e}");
                        }
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {
                    // tungstenite 自动处理 Ping/Pong
                }
                _ => {}
            }
        }
        tracing::info!("ws receiver exited");
    };

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    pty_read_handle.abort();
    Ok(())
}

/// 处理一个帧，返回 false 表示需要关闭会话
fn handle_frame(
    frame: Frame,
    pty_writer: &mut Box<dyn Write + Send>,
    master: &Box<dyn portable_pty::MasterPty + Send>,
) -> bool {
    match frame.frame_type {
        FrameType::Data => {
            if let Err(e) = pty_writer.write_all(&frame.payload) {
                tracing::warn!("pty write error: {e}");
                return false;
            }
            let _ = pty_writer.flush();
        }
        FrameType::Resize => {
            if let Some((cols, rows)) = frame.parse_resize() {
                tracing::debug!("resize: {cols}x{rows}");
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
        }
        FrameType::Heartbeat | FrameType::HeartbeatAck => {}
        FrameType::Close => {
            tracing::info!("server requested close");
            return false;
        }
    }
    true
}

fn default_shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
    }
}
