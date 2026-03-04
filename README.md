# gm-tty

轻量级 **Web 终端 + 内网穿透** 工具。基于 WebSocket 协议，所有流量经过自建 Server 中转，无需 P2P 打洞。

类似 `ttyd` + `frp` 的结合体，单二进制部署，完全用 Rust 实现。

---

## 架构

```
浏览器 (xterm.js)  ←──WSS──→  Server (公网)  ←──WSS──→  Agent (内网)  ←──PTY──→  Shell
```

- **Server**：公网部署，提供 HTTPS Web 前端，管理会话，中转数据
- **Agent**：内网运行，主动反向连接 Server，管理本地 Shell PTY

---

## 快速开始

### 1. 编译

```bash
cargo build --release
# 产出：
#   target/release/gm-tty-server
#   target/release/gm-tty-agent
```

### 2. 启动 Server（公网机器）

```bash
# 自动生成临时 Token（开发测试用）
./gm-tty-server

# 输出示例：
# WARN  temp token: f7d1ba96...
# WARN  web url:    http://0.0.0.0:7093/terminal/f7d1ba96...
# WARN  agent env:  GM_TTY_SERVER=ws://0.0.0.0:7093 GM_TTY_TOKEN=f7d1ba96...
```

也可通过配置文件管理 Token：

```toml
# server.toml
listen_addr = "0.0.0.0:7093"
tokens = [
    "your-token-here",
]
```

```bash
./gm-tty-server server.toml
```

或通过环境变量：

```bash
GM_TTY_TOKENS="token1,token2" ./gm-tty-server
```

生成随机 Token：

```bash
./gm-tty-server gen-token
```

### 3. 启动 Agent（内网机器）

```bash
export GM_TTY_SERVER="ws://your-server-ip:7093"
export GM_TTY_TOKEN="your-token-here"
./gm-tty-agent
```

### 4. 打开浏览器

访问 `http://your-server-ip:7093/terminal/your-token-here`

---

## 通信协议

WebSocket Binary 帧格式（TLV）：

```
[Type: 1B] [Length: 4B BigEndian] [Payload: NB]
```

| Type | 含义 |
|------|------|
| `0x01` | Data - 终端数据 |
| `0x02` | Resize - 窗口大小 `{cols: u16, rows: u16}` |
| `0x03` | Heartbeat |
| `0x04` | HeartbeatAck |
| `0x05` | Close |

---

## Roadmap

- [x] **Phase 1 MVP**：Token 认证，WebSocket 中转，PTY 管理，Web 终端
- [ ] **Phase 2**：TLS/WSS，断线重连增强，心跳超时，Token CLI，TOML 配置
- [ ] **Phase 3**：多路复用，文件传输，会话录制，TLCP 国密

---

## 许可证

MIT
