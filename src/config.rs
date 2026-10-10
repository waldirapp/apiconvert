//! Configuração por variáveis de ambiente. Falha fechada: sem token interno
//! válido o serviço não sobe.

use std::net::SocketAddr;
use std::time::Duration;

use crate::convert::Limits;

/// Tamanho mínimo do token interno.
pub const MIN_TOKEN_LEN: usize = 32;

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub token: String,
    pub max_body_bytes: usize,
    pub max_concurrent: usize,
    pub timeout: Duration,
    pub limits: Limits,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // O token nunca aparece em log.
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("token", &"<oculto>")
            .field("max_body_bytes", &self.max_body_bytes)
            .field("max_concurrent", &self.max_concurrent)
            .field("timeout", &self.timeout)
            .field("limits", &self.limits)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("XLSCONVERT_TOKEN ausente ou com menos de {MIN_TOKEN_LEN} caracteres")]
    Token,
    #[error("valor inválido em {0}")]
    Invalid(&'static str),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let token = get("XLSCONVERT_TOKEN").unwrap_or_default();
        if token.trim().len() < MIN_TOKEN_LEN || token.trim() != token {
            return Err(ConfigError::Token);
        }
        let bind = parse_or(&get, "XLSCONVERT_BIND", "127.0.0.1:9320")?;
        let max_body_mb: usize = parse_or(&get, "XLSCONVERT_MAX_BODY_MB", "20")?;
        let max_concurrent: usize = parse_or(&get, "XLSCONVERT_MAX_CONCURRENT", "2")?;
        let timeout_secs: u64 = parse_or(&get, "XLSCONVERT_TIMEOUT_SECS", "60")?;
        let max_cells: u64 = parse_or(&get, "XLSCONVERT_MAX_CELLS", "5000000")?;
        if max_body_mb == 0 || max_concurrent == 0 || timeout_secs == 0 || max_cells == 0 {
            return Err(ConfigError::Invalid(
                "limites (precisam ser maiores que zero)",
            ));
        }
        Ok(Self {
            bind,
            token,
            max_body_bytes: max_body_mb * 1024 * 1024,
            max_concurrent,
            timeout: Duration::from_secs(timeout_secs),
            limits: Limits {
                max_cells,
                ..Limits::default()
            },
        })
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
}
