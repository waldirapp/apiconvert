//! Serviço interno de conversão de planilha `.xls`/`.xlsx` → `.xlsx`
//! que reproduz o resultado de `pandas.read_excel` + `DataFrame.to_excel`.
//!
//! - `convert`: conversão pura, com a semântica do pandas 2.2.3 usada hoje.
//! - `app`: rotas HTTP do modo interno (`/health`, `POST /convert`), para uso
//!   só pelo serviço chamador na mesma máquina, com token interno obrigatório.
//! - `compat`: modo `apiconvert`, com o contrato HTTP do conversor legado.
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

pub mod app;
pub mod compat;
pub mod config;
pub mod convert;
