//! Ponto de entrada do serviço `xlsconvert`.

use tracing_subscriber::EnvFilter;
use xlsconvert::config::{Config, Mode};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("XLSCONVERT_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        // Sem códigos de cor no journal do systemd: só colore em terminal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("configuração inválida: {e}");
            std::process::exit(2);
        }
    };
    if cfg.mode == Mode::Interno && !cfg.bind.ip().is_loopback() {
        tracing::warn!(bind = %cfg.bind, "escutando fora do loopback: o serviço deve ficar atrás do serviço chamador e do firewall");
    }
    if cfg.mode == Mode::Apiconvert {
        tracing::warn!(bind = %cfg.bind, "modo apiconvert: contrato legado sem autenticação; restrinja o acesso pelo firewall");
    }

    let listener = match tokio::net::TcpListener::bind(cfg.bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(bind = %cfg.bind, "não foi possível abrir a porta: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!(bind = %cfg.bind, modo = ?cfg.mode, versao = env!("CARGO_PKG_VERSION"), "xlsconvert iniciado");

    if let Err(e) = axum::serve(listener, xlsconvert::router(cfg))
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("servidor encerrado com erro: {e}");
        std::process::exit(1);
    }
    tracing::info!("xlsconvert encerrado");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
