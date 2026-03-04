use anyhow::Result;
use gm_tty_server::{Config, generate_token};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("gm_tty_server=info".parse()?)
                .add_directive("tower_http=info".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    // 子命令: `gm-tty-server gen-token` 生成一个随机 Token
    if args.get(1).map(|s| s.as_str()) == Some("gen-token") {
        println!("{}", generate_token());
        return Ok(());
    }

    let config_path = args.get(1).map(|s| s.as_str()).unwrap_or("server.toml");
    let mut config = Config::load(config_path)?;

    // 支持环境变量 GM_TTY_TOKENS 覆盖（逗号分隔）
    if let Ok(tokens_env) = std::env::var("GM_TTY_TOKENS") {
        config.tokens = tokens_env.split(',').map(|s| s.trim().to_string()).collect();
    }

    if config.tokens.is_empty() {
        tracing::warn!("no tokens configured, generating a temporary token for testing");
        let token = generate_token();
        tracing::warn!("temp token: {token}");
        tracing::warn!(
            "web url:    http://{}/terminal/{token}",
            config.listen_addr
        );
        tracing::warn!(
            "agent env:  GM_TTY_SERVER=ws://{} GM_TTY_TOKEN={token}",
            config.listen_addr
        );
        config.tokens.push(token);
    } else {
        for token in &config.tokens {
            tracing::info!(
                "token ready: http://{}/terminal/{token}",
                config.listen_addr
            );
        }
    }

    gm_tty_server::run(config).await
}
