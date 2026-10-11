//! Configuração por variáveis de ambiente.
//!
//! Dois modos (`XLSCONVERT_MODE`):
//! - `interno` (padrão): `POST /convert` com os bytes da planilha e resposta
//!   com o `.xlsx`, protegido por token interno obrigatório (falha fechada:
//!   sem token válido o serviço não sobe).
//! - `apiconvert`: contrato HTTP idêntico ao do conversor legado
//!   (multipart + download por nome), para substituí-lo sem mudar os
//!   clientes. Os clientes legados não enviam credencial, então esse modo não
//!   tem token; ver `compat.rs`.
//!
//! A rota `POST /webp` (`imagem.rs`) exige token nos dois modos: no `interno`
//! é o `XLSCONVERT_TOKEN`; no `apiconvert` é o `XLSCONVERT_WEBP_TOKEN`, e sem
//! ele a rota não é registrada.

use std::net::SocketAddr;
use std::time::Duration;

use crate::convert::Limits;

/// Tamanho mínimo do token interno.
pub const MIN_TOKEN_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Interno,
    Apiconvert,
}

/// Parâmetros do armazenamento temporário do modo `apiconvert`.
#[derive(Debug, Clone, Copy)]
pub struct CompatStore {
    /// Tempo que um `.xlsx` convertido fica disponível para `GET`.
    pub ttl: Duration,
    /// Teto de bytes somados de todos os arquivos guardados.
    pub max_bytes: usize,
    /// Teto de arquivos guardados ao mesmo tempo.
    pub max_entries: usize,
    /// O `GET` só entrega o resultado ao IP que fez o `POST`.
    pub bind_ip: bool,
    /// O resultado sai do armazenamento no primeiro `GET` que o entrega.
    pub one_time: bool,
    /// Nomes inexistentes por IP, por minuto, antes de o `GET` parar de
    /// consultar o armazenamento para esse IP.
    pub max_misses_per_min: u32,
}

impl Default for CompatStore {
    fn default() -> Self {
        Self {
            ttl: Duration::from_secs(300),
            max_bytes: 256 * 1024 * 1024,
            max_entries: 2000,
            bind_ip: true,
            one_time: true,
            max_misses_per_min: 30,
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub mode: Mode,
    pub bind: SocketAddr,
    pub token: String,
    pub max_body_bytes: usize,
    pub max_concurrent: usize,
    /// Requisições de conversão admitidas, incluindo upload/fila/resposta.
    pub max_inflight: usize,
    /// Cota por IP do peer; menor que a global quando há mais de uma vaga.
    pub max_inflight_per_ip: usize,
    pub upload_timeout: Duration,
    /// Executável do worker em produção; None para uso em processo da biblioteca.
    pub worker_path: Option<std::path::PathBuf>,
    pub worker_memory_bytes: u64,
    pub timeout: Duration,
    pub limits: Limits,
    pub compat: CompatStore,
    /// Token da rota `/webp`; `None` desliga a rota.
    pub webp_token: Option<String>,
    /// Teto de largura × altura da imagem decodificada em `/webp`.
    pub webp_max_pixels: u64,
    /// Telas web (`/`, `/planilha`, `/imagem`); ver `ui.rs`.
    pub ui: bool,
    /// Conversões por minuto, por IP, nas telas.
    pub ui_rate_per_min: u32,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // O token nunca aparece em log.
        f.debug_struct("Config")
            .field("mode", &self.mode)
            .field("bind", &self.bind)
            .field("token", &"<oculto>")
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_concurrent", &self.max_concurrent)
            .field("max_inflight", &self.max_inflight)
            .field("max_inflight_per_ip", &self.max_inflight_per_ip)
            .field("upload_timeout", &self.upload_timeout)
            .field("worker_memory_bytes", &self.worker_memory_bytes)
            .field("timeout", &self.timeout)
            .field("limits", &self.limits)
            .field("compat", &self.compat)
            .field("webp_token", &self.webp_token.as_ref().map(|_| "<oculto>"))
            .field("webp_max_pixels", &self.webp_max_pixels)
            .field("ui", &self.ui)
            .field("ui_rate_per_min", &self.ui_rate_per_min)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("XLSCONVERT_TOKEN ausente ou com menos de {MIN_TOKEN_LEN} caracteres")]
    Token,
    #[error("XLSCONVERT_WEBP_TOKEN com menos de {MIN_TOKEN_LEN} caracteres")]
    WebpToken,
    #[error("valor inválido em {0}")]
    Invalid(&'static str),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut cfg = Self::from_lookup(|k| std::env::var(k).ok())?;
        cfg.worker_path = Some(
            std::env::current_exe().map_err(|_| ConfigError::Invalid("executável do worker"))?,
        );
        Ok(cfg)
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mode = match get("XLSCONVERT_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "interno" => Mode::Interno,
            "apiconvert" => Mode::Apiconvert,
            _ => return Err(ConfigError::Invalid("XLSCONVERT_MODE")),
        };

        let token = get("XLSCONVERT_TOKEN").unwrap_or_default();
        if mode == Mode::Interno && !token_valid(&token) {
            return Err(ConfigError::Token);
        }
        let webp_token = match mode {
            Mode::Interno => Some(token.clone()),
            Mode::Apiconvert => match get("XLSCONVERT_WEBP_TOKEN") {
                None => None,
                Some(t) if t.is_empty() => None,
                Some(t) if token_valid(&t) => Some(t),
                Some(_) => return Err(ConfigError::WebpToken),
            },
        };

        let default_bind = match mode {
            Mode::Interno => "127.0.0.1:9320",
            Mode::Apiconvert => "0.0.0.0:9098",
        };
        let bind = parse_or(&get, "XLSCONVERT_BIND", default_bind)?;
        let max_body_mb: usize = parse_or(&get, "XLSCONVERT_MAX_BODY_MB", "20")?;
        let max_concurrent: usize = parse_or(&get, "XLSCONVERT_MAX_CONCURRENT", "2")?;
        let default_inflight = max_concurrent
            .checked_add(2)
            .ok_or(ConfigError::Invalid("XLSCONVERT_MAX_CONCURRENT"))?;
        let max_inflight: usize = parse_or(
            &get,
            "XLSCONVERT_MAX_INFLIGHT",
            &default_inflight.to_string(),
        )?;
        let upload_timeout_secs: u64 = parse_or(&get, "XLSCONVERT_UPLOAD_TIMEOUT_SECS", "30")?;
        let default_per_ip = max_inflight.saturating_sub(1).clamp(1, 2);
        let max_inflight_per_ip: usize = parse_or(
            &get,
            "XLSCONVERT_MAX_INFLIGHT_PER_IP",
            &default_per_ip.to_string(),
        )?;
        // XLSX lê células incrementalmente, mas materializa a grade inteira.
        // MAX_CELLS é um teto, não garantia de aceitação: expansão/texto e
        // memória virtual do worker são limites independentes (sem derivação).
        let max_expanded_mb: u64 = parse_or(&get, "XLSCONVERT_MAX_EXPANDED_MB", "64")?;
        let worker_memory_mb: u64 = parse_or(&get, "XLSCONVERT_WORKER_MEMORY_MB", "128")?;
        let timeout_secs: u64 = parse_or(&get, "XLSCONVERT_TIMEOUT_SECS", "60")?;
        let max_cells: u64 = parse_or(&get, "XLSCONVERT_MAX_CELLS", "5000000")?;
        // O calamine abre todas as abas XLS, inclusive gráficos/VB. Esse
        // orçamento protege o parser sem somar abas à cota da primeira planilha.
        let max_xls_parse_cells: u64 = parse_or(&get, "XLSCONVERT_MAX_XLS_PARSE_CELLS", "5000000")?;
        let ttl_secs: u64 = parse_or(&get, "XLSCONVERT_COMPAT_TTL_SECS", "300")?;
        let bind_ip = flag(&get, "XLSCONVERT_COMPAT_BIND_IP", true)?;
        let one_time = flag(&get, "XLSCONVERT_COMPAT_ONE_TIME", true)?;
        let max_misses_per_min: u32 = parse_or(&get, "XLSCONVERT_COMPAT_MAX_MISSES_PER_MIN", "30")?;
        let store_mb: usize = parse_or(&get, "XLSCONVERT_COMPAT_MAX_STORE_MB", "256")?;
        let max_entries: usize = parse_or(&get, "XLSCONVERT_COMPAT_MAX_ENTRIES", "2000")?;
        let webp_max_pixels: u64 = parse_or(&get, "XLSCONVERT_WEBP_MAX_PIXELS", "25000000")?;
        // Telas: ligadas por padrão no modo apiconvert (já exposto), desligadas
        // no interno (só para o serviço chamador).
        let ui_padrao = if mode == Mode::Apiconvert { "1" } else { "0" };
        let ui = flag(&get, "XLSCONVERT_UI", ui_padrao == "1")?;
        let ui_rate_per_min: u32 = parse_or(&get, "XLSCONVERT_UI_RATE_PER_MIN", "30")?;
        if max_body_mb == 0
            || max_concurrent == 0
            || max_concurrent > tokio::sync::Semaphore::MAX_PERMITS
            || max_inflight == 0
            || max_inflight > tokio::sync::Semaphore::MAX_PERMITS
            || max_inflight_per_ip == 0
            || max_inflight_per_ip > max_inflight
            || (max_inflight > 1 && max_inflight_per_ip == max_inflight)
            || upload_timeout_secs == 0
            || max_expanded_mb == 0
            || worker_memory_mb == 0
            || timeout_secs == 0
            || max_cells == 0
            || max_xls_parse_cells == 0
            || ttl_secs == 0
            || max_misses_per_min == 0
            || store_mb == 0
            || max_entries == 0
            || webp_max_pixels == 0
            || ui_rate_per_min == 0
        {
            return Err(ConfigError::Invalid(
                "limites (precisam ser maiores que zero)",
            ));
        }
        Ok(Self {
            mode,
            bind,
            token,
            max_body_bytes: max_body_mb
                .checked_mul(1024 * 1024)
                .ok_or(ConfigError::Invalid("XLSCONVERT_MAX_BODY_MB"))?,
            max_concurrent,
            max_inflight,
            max_inflight_per_ip,
            upload_timeout: Duration::from_secs(upload_timeout_secs),
            worker_path: None,
            worker_memory_bytes: worker_memory_mb
                .checked_mul(1024 * 1024)
                .ok_or(ConfigError::Invalid("XLSCONVERT_WORKER_MEMORY_MB"))?,
            timeout: Duration::from_secs(timeout_secs),
            limits: Limits {
                max_cells,
                max_xls_parse_cells,
                max_expanded_bytes: max_expanded_mb
                    .checked_mul(1024 * 1024)
                    .ok_or(ConfigError::Invalid("XLSCONVERT_MAX_EXPANDED_MB"))?,
                ..Limits::default()
            },
            compat: CompatStore {
                ttl: Duration::from_secs(ttl_secs),
                max_bytes: store_mb * 1024 * 1024,
                max_entries,
                bind_ip,
                one_time,
                max_misses_per_min,
            },
            webp_token,
            webp_max_pixels,
            ui,
            ui_rate_per_min,
        })
    }
}

fn token_valid(t: &str) -> bool {
    t.trim().len() >= MIN_TOKEN_LEN && t.trim() == t
}

fn flag(
    get: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: bool,
) -> Result<bool, ConfigError> {
    match get(key) {
        None => Ok(default),
        Some(v) => match v.trim() {
            "1" | "true" | "sim" => Ok(true),
            "0" | "false" | "nao" | "não" => Ok(false),
            _ => Err(ConfigError::Invalid(key)),
        },
    }
}

fn parse_or<T: std::str::FromStr>(
    get: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: &str,
) -> Result<T, ConfigError> {
    get(key)
        .unwrap_or_else(|| default.to_string())
        .trim()
        .parse()
        .map_err(|_| ConfigError::Invalid(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cota_por_ip_reserva_capacidade_para_outros_clientes() {
        for (global, expected) in [("1", 1), ("2", 1), ("4", 2)] {
            let cfg = Config::from_lookup(lookup(&[
                ("XLSCONVERT_MODE", "apiconvert"),
                ("XLSCONVERT_MAX_INFLIGHT", global),
            ]))
            .unwrap();
            assert_eq!(cfg.max_inflight_per_ip, expected);
        }
        assert!(Config::from_lookup(lookup(&[
            ("XLSCONVERT_MODE", "apiconvert"),
            ("XLSCONVERT_MAX_INFLIGHT", "4"),
            ("XLSCONVERT_MAX_INFLIGHT_PER_IP", "4")
        ]))
        .is_err());
    }

    #[test]
    fn novos_limites_invalidos_nao_iniciam_servico() {
        for key in [
            "XLSCONVERT_MAX_INFLIGHT",
            "XLSCONVERT_MAX_INFLIGHT_PER_IP",
            "XLSCONVERT_MAX_XLS_PARSE_CELLS",
            "XLSCONVERT_UPLOAD_TIMEOUT_SECS",
            "XLSCONVERT_MAX_EXPANDED_MB",
            "XLSCONVERT_WORKER_MEMORY_MB",
        ] {
            assert!(
                Config::from_lookup(lookup(&[("XLSCONVERT_MODE", "apiconvert"), (key, "0")]))
                    .is_err()
            );
        }
        assert!(Config::from_lookup(lookup(&[
            ("XLSCONVERT_MODE", "apiconvert"),
            ("XLSCONVERT_MAX_INFLIGHT", "18446744073709551615")
        ]))
        .is_err());
    }

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| owned.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn sem_token_nao_sobe() {
        assert!(matches!(
            Config::from_lookup(lookup(&[])),
            Err(ConfigError::Token)
        ));
        assert!(matches!(
            Config::from_lookup(lookup(&[("XLSCONVERT_TOKEN", "curto")])),
            Err(ConfigError::Token)
        ));
    }

    #[test]
    fn padroes_seguros() {
        let tok = "x".repeat(MIN_TOKEN_LEN);
        let cfg = Config::from_lookup(lookup(&[("XLSCONVERT_TOKEN", &tok)])).unwrap();
        assert_eq!(cfg.mode, Mode::Interno);
        assert!(cfg.bind.ip().is_loopback());
        assert_eq!(cfg.bind.port(), 9320);
        assert_eq!(cfg.max_body_bytes, 20 * 1024 * 1024);
        assert!(!format!("{cfg:?}").contains(&tok));
    }

    #[test]
    fn limite_zero_recusado() {
        let tok = "x".repeat(MIN_TOKEN_LEN);
        assert!(Config::from_lookup(lookup(&[
            ("XLSCONVERT_TOKEN", &tok),
            ("XLSCONVERT_MAX_CONCURRENT", "0")
        ]))
        .is_err());
    }

    #[test]
    fn token_do_webp_por_modo() {
        let tok = "x".repeat(MIN_TOKEN_LEN);
        let cfg = Config::from_lookup(lookup(&[("XLSCONVERT_TOKEN", &tok)])).unwrap();
        assert_eq!(cfg.webp_token.as_deref(), Some(tok.as_str()));

        let cfg = Config::from_lookup(lookup(&[("XLSCONVERT_MODE", "apiconvert")])).unwrap();
        assert_eq!(cfg.webp_token, None);
        let cfg = Config::from_lookup(lookup(&[
            ("XLSCONVERT_MODE", "apiconvert"),
            ("XLSCONVERT_WEBP_TOKEN", &tok),
        ]))
        .unwrap();
        assert_eq!(cfg.webp_token.as_deref(), Some(tok.as_str()));
        assert!(!format!("{cfg:?}").contains(&tok));
        assert!(matches!(
            Config::from_lookup(lookup(&[
                ("XLSCONVERT_MODE", "apiconvert"),
                ("XLSCONVERT_WEBP_TOKEN", "curto"),
            ])),
            Err(ConfigError::WebpToken)
        ));
    }

    #[test]
    fn modo_apiconvert_sem_token_e_porta_legada() {
        let cfg = Config::from_lookup(lookup(&[("XLSCONVERT_MODE", "apiconvert")])).unwrap();
        assert_eq!(cfg.mode, Mode::Apiconvert);
        assert_eq!(cfg.bind.port(), 9098);
        // Download protegido por padrão (GHSA-jc42-3ccg-wf85).
        assert_eq!(cfg.compat.ttl, Duration::from_secs(300));
        assert!(cfg.compat.bind_ip && cfg.compat.one_time);
        assert_eq!(cfg.compat.max_misses_per_min, 30);
        let aberto = Config::from_lookup(lookup(&[
            ("XLSCONVERT_MODE", "apiconvert"),
            ("XLSCONVERT_COMPAT_BIND_IP", "0"),
            ("XLSCONVERT_COMPAT_ONE_TIME", "0"),
        ]))
        .unwrap();
        assert!(!aberto.compat.bind_ip && !aberto.compat.one_time);
        assert!(Config::from_lookup(lookup(&[
            ("XLSCONVERT_MODE", "apiconvert"),
            ("XLSCONVERT_COMPAT_BIND_IP", "talvez"),
        ]))
        .is_err());
    }

    #[test]
    fn modo_desconhecido_recusado() {
        assert!(matches!(
            Config::from_lookup(lookup(&[("XLSCONVERT_MODE", "outro")])),
            Err(ConfigError::Invalid("XLSCONVERT_MODE"))
        ));
    }
}
