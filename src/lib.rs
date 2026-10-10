//! Serviço interno de conversão de planilha `.xls`/`.xlsx` → `.xlsx`
//! que reproduz o resultado de `pandas.read_excel` + `DataFrame.to_excel`.
//!
//! - `convert`: conversão pura, com a semântica do pandas 2.2.3 usada hoje.
//! - `app`: rotas HTTP (`/health`, `POST /convert`), para uso só pelo
//!   serviço chamador na mesma máquina, com token interno obrigatório.

pub mod app;
pub mod config;
pub mod convert;
