// gm-tty frontend - WebSocket terminal client
// Protocol: Binary TLV frames [Type:1B][Length:4B BE][Payload:NB]

const FRAME_TYPE = {
    DATA: 0x01,
    RESIZE: 0x02,
    HEARTBEAT: 0x03,
    HEARTBEAT_ACK: 0x04,
    CLOSE: 0x05,
};

const HEARTBEAT_INTERVAL_MS = 30000;
const HEARTBEAT_TIMEOUT_MS = 90000;

// --- Token 从 URL path 提取: /terminal/{token} ---
const pathParts = window.location.pathname.split('/');
const token = pathParts[pathParts.length - 1];

// --- 终端初始化 ---
const term = new Terminal({
    cursorBlink: true,
    fontSize: 14,
    fontFamily: 'Consolas, "Courier New", monospace',
    theme: {
        background: '#1e1e1e',
        foreground: '#d4d4d4',
        cursor: '#d4d4d4',
        selectionBackground: '#264f78',
        black: '#1e1e1e',
        brightBlack: '#808080',
        red: '#f44747',
        brightRed: '#f44747',
        green: '#6a9955',
        brightGreen: '#6a9955',
        yellow: '#dcdcaa',
        brightYellow: '#dcdcaa',
        blue: '#569cd6',
        brightBlue: '#569cd6',
        magenta: '#c586c0',
        brightMagenta: '#c586c0',
        cyan: '#4ec9b0',
        brightCyan: '#4ec9b0',
        white: '#d4d4d4',
        brightWhite: '#ffffff',
    },
    allowTransparency: false,
    scrollback: 10000,
});

const fitAddon = new FitAddon.FitAddon();
term.loadAddon(fitAddon);
term.open(document.getElementById('terminal'));
fitAddon.fit();

// --- 状态栏 ---
const statusBar = document.getElementById('status-bar');

function setStatus(state, text) {
    statusBar.className = state;
    statusBar.textContent = text;
}

// --- 帧编解码 ---
function encodeFrame(type, payload) {
    const payloadBytes = payload instanceof Uint8Array ? payload : new Uint8Array(0);
    const buf = new ArrayBuffer(5 + payloadBytes.length);
    const view = new DataView(buf);
    view.setUint8(0, type);
    view.setUint32(1, payloadBytes.length, false); // big-endian
    new Uint8Array(buf).set(payloadBytes, 5);
    return buf;
}

function decodeFrame(data) {
    if (data.length < 5) return null;
    const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
    const type = view.getUint8(0);
    const length = view.getUint32(1, false);
    if (data.length < 5 + length) return null;
    return {
        type,
        payload: data.slice(5, 5 + length),
    };
}

function encodeResizeFrame(cols, rows) {
    const payload = new Uint8Array(4);
    const view = new DataView(payload.buffer);
    view.setUint16(0, cols, false);
    view.setUint16(2, rows, false);
    return encodeFrame(FRAME_TYPE.RESIZE, payload);
}

// --- WebSocket 连接 ---
let ws = null;
let heartbeatTimer = null;
let heartbeatTimeoutTimer = null;
let reconnectTimer = null;
let reconnectDelay = 1000;
const MAX_RECONNECT_DELAY = 30000;

function connect() {
    if (ws && ws.readyState === WebSocket.OPEN) return;

    setStatus('connecting', '● connecting...');

    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const url = `${proto}//${location.host}/ws/web/${token}`;

    ws = new WebSocket(url);
    ws.binaryType = 'arraybuffer';

    ws.onopen = () => {
        setStatus('connected', '● connected');
        reconnectDelay = 1000;

        // 发送初始窗口大小
        sendResize(term.cols, term.rows);

        // 启动心跳
        startHeartbeat();
    };

    ws.onmessage = (evt) => {
        const data = new Uint8Array(evt.data);
        const frame = decodeFrame(data);
        if (!frame) return;

        switch (frame.type) {
            case FRAME_TYPE.DATA:
                term.write(frame.payload);
                break;
            case FRAME_TYPE.HEARTBEAT:
                ws.send(encodeFrame(FRAME_TYPE.HEARTBEAT_ACK, new Uint8Array(0)));
                break;
            case FRAME_TYPE.HEARTBEAT_ACK:
                clearTimeout(heartbeatTimeoutTimer);
                break;
            case FRAME_TYPE.CLOSE:
                term.write('\r\n\x1b[33m[session closed]\x1b[0m\r\n');
                ws.close();
                break;
        }
    };

    ws.onclose = (evt) => {
        stopHeartbeat();
        if (evt.code === 4001) {
            setStatus('disconnected', '● invalid token');
            term.write('\r\n\x1b[31m[invalid token]\x1b[0m\r\n');
            return;
        }
        if (evt.code === 4002) {
            setStatus('disconnected', '● agent not connected');
            term.write('\r\n\x1b[33m[agent not connected, retrying...]\x1b[0m\r\n');
        } else {
            setStatus('disconnected', `● disconnected (${reconnectDelay / 1000}s)`);
        }

        scheduleReconnect();
    };

    ws.onerror = () => {
        // onclose 会紧接着触发
    };
}

function scheduleReconnect() {
    clearTimeout(reconnectTimer);
    reconnectTimer = setTimeout(() => {
        connect();
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, MAX_RECONNECT_DELAY);
}

function sendResize(cols, rows) {
    if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(encodeResizeFrame(cols, rows));
    }
}

function startHeartbeat() {
    stopHeartbeat();
    heartbeatTimer = setInterval(() => {
        if (ws && ws.readyState === WebSocket.OPEN) {
            ws.send(encodeFrame(FRAME_TYPE.HEARTBEAT, new Uint8Array(0)));
            heartbeatTimeoutTimer = setTimeout(() => {
                ws.close();
            }, HEARTBEAT_TIMEOUT_MS);
        }
    }, HEARTBEAT_INTERVAL_MS);
}

function stopHeartbeat() {
    clearInterval(heartbeatTimer);
    clearTimeout(heartbeatTimeoutTimer);
}

// --- 用户输入 ---
term.onData((data) => {
    if (ws && ws.readyState === WebSocket.OPEN) {
        const encoded = new TextEncoder().encode(data);
        ws.send(encodeFrame(FRAME_TYPE.DATA, encoded));
    }
});

// --- 窗口 Resize ---
const resizeObserver = new ResizeObserver(() => {
    fitAddon.fit();
});
resizeObserver.observe(document.getElementById('terminal-container'));

term.onResize(({ cols, rows }) => {
    sendResize(cols, rows);
});

// --- 启动 ---
connect();
