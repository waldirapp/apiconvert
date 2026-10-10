//! Serviço interno de conversão de planilha `.xls`/`.xlsx` → `.xlsx`
//! que reproduz o resultado de `pandas.read_excel` + `DataFrame.to_excel`.
//!
//! - `convert`: conversão pura, com a semântica do pandas 2.2.3 usada hoje.
//! - `app`: rotas HTTP do modo interno (`/health`, `POST /convert`), para uso
//!   só pelo serviço chamador na mesma máquina, com token interno obrigatório.
//! - `compat`: modo `apiconvert`, com o contrato HTTP do conversor legado.
//! - `webp` e `imagem`: conversão de imagem em WebP (`POST /webp`, com token),
//!   nos dois modos.
//! - `ui`: telas web (askama + htmx) para testar as duas conversões.
//!
//! `router` escolhe as rotas pelo modo da configuração.

use axum::Router;

/// Rotas do modo configurado (`XLSCONVERT_MODE`).
pub fn router(cfg: config::Config) -> Router {
    match cfg.mode {
        config::Mode::Interno => app::router(cfg),
        config::Mode::Apiconvert => compat::router(cfg),
    }
}

/// Rota `/webp`, quando há token para ela, com o semáforo das planilhas.
pub(crate) fn imagem_router(
    cfg: &config::Config,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
) -> Option<Router> {
    let token = cfg.webp_token.as_deref()?;
    Some(imagem::router(
        token,
        cfg.max_body_bytes,
        permits,
        cfg.timeout,
        cfg.webp_max_pixels,
    ))
}

/// Telas web, quando ligadas, com o semáforo das conversões.
pub(crate) fn ui_router(
    cfg: &config::Config,
    permits: std::sync::Arc<tokio::sync::Semaphore>,
) -> Option<Router> {
    cfg.ui.then(|| ui::router(cfg, permits))
}

mod admission;
pub mod app;
pub mod compat;
pub mod config;
pub mod conversion_worker;
pub mod convert;
pub mod imagem;
pub mod ui;
pub mod webp;
