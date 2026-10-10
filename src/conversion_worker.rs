//! Worker sem rede, do mesmo binário. IPC usa stdin/stdout, sem arquivos
//! temporários ou conteúdo em logs. O servidor continua vivo após abort/OOM.
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Arc;

use axum::body::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;

use crate::config::Config;
use crate::convert::{self, ConvertError, Limits};

const LIMIT_REASONS: &[&str] = &[
    "recursos",
    "linhas",
    "colunas",
    "células",
    "entradas ZIP",
    "expansão ZIP",
    "texto",
    "fluxo XLS",
    "células XLS",
    "abas XLS",
    "upload",
    "saída",
];

pub(crate) async fn run(
    bytes: Bytes,
    limits: Limits,
    cfg: &Config,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<Vec<u8>, ConvertError> {
    let Some(path) = &cfg.worker_path else {
        return tokio::task::spawn_blocking(move || {
            let _permit = permit;
            convert::convert(&bytes, limits)
        })
        .await
        .map_err(|_| ConvertError::Write)?;
    };
    let _permit = permit;
    let mut child = tokio::process::Command::new(path)
        .args([
            "--conversion-worker",
            &limits.max_cells.to_string(),
            &limits.max_expanded_bytes.to_string(),
            &cfg.max_body_bytes.to_string(),
            &cfg.timeout.as_secs().to_string(),
            &(cfg.worker_memory_bytes / (1024 * 1024)).to_string(),
        ])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ConvertError::Write)?;
    let operation = async {
        let mut stdin = child.stdin.take().ok_or(ConvertError::Write)?;
        // O worker lê a entrada inteira antes de escrever a saída: não há
        // deadlock entre os pipes de entrada/saída.
        if stdin.write_all(&bytes).await.is_err() {
            return Err(ConvertError::TooLarge("recursos do parser"));
        }
        drop(stdin);
        let mut stdout = child.stdout.take().ok_or(ConvertError::Write)?;
        let mut header = [0u8; 7];
        if stdout.read_exact(&mut header).await.is_err() {
            return Err(ConvertError::TooLarge("recursos do parser"));
        }
        let result = if header[0] == 0 {
            let mut out = Vec::new();
            stdout
                .take((cfg.compat.max_bytes as u64).saturating_add(1))
                .read_to_end(&mut out)
                .await
                .map_err(|_| ConvertError::Write)?;
            if out.len() > cfg.compat.max_bytes {
                return Err(ConvertError::TooLarge("saída"));
            }
            Ok(out)
        } else {
            Err(decode_error(header))
        };
        let status = child.wait().await.map_err(|_| ConvertError::Write)?;
        if !status.success() {
            return Err(ConvertError::TooLarge("recursos do parser"));
        }
        result
    };
    match tokio::time::timeout(cfg.timeout, operation).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            Err(ConvertError::DeadlineExceeded)
        }
    }
}

/// Chamado antes de iniciar Tokio/logging, evitando threads e qualquer bind.
/// Parâmetros são apenas limites numéricos; o worker não recebe credenciais.
pub fn worker_entry() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--conversion-worker") {
        return None;
    }
    let run = || -> Result<(), ()> {
        if args.len() != 7 {
            return Err(());
        }
        let numeric: Vec<u64> = args[2..]
            .iter()
            .map(|s| s.parse().map_err(|_| ()))
            .collect::<Result<_, _>>()?;
        let [max_cells, max_expanded_bytes, max_input, timeout, memory_mb] = numeric.as_slice()
        else {
            return Err(());
        };
        let memory = memory_mb.checked_mul(1024 * 1024).ok_or(())?;
        set_memory_limit(memory)?;
        let limits = Limits {
            max_cells: *max_cells,
            max_expanded_bytes: *max_expanded_bytes,
            ..Limits::default()
        }
        .with_timeout(std::time::Duration::from_secs(*timeout));
        let mut input = Vec::new();
        std::io::stdin()
            .take(max_input.checked_add(1).ok_or(())?)
            .read_to_end(&mut input)
            .map_err(|_| ())?;
        let result = if input.len() as u64 > *max_input {
            Err(ConvertError::TooLarge("upload"))
        } else {
            convert::convert(&input, limits)
        };
        let mut stdout = std::io::stdout().lock();
        match result {
            Ok(out) => {
                stdout.write_all(&[0u8; 7]).map_err(|_| ())?;
                stdout.write_all(&out).map_err(|_| ())?;
            }
            Err(e) => stdout.write_all(&encode_error(e)).map_err(|_| ())?,
        }
        stdout.flush().map_err(|_| ())
    };
    Some(if run().is_ok() { 0 } else { 2 })
}

#[cfg(unix)]
fn set_memory_limit(bytes: u64) -> Result<(), ()> {
    let limit = libc::rlimit {
        rlim_cur: bytes as libc::rlim_t,
        rlim_max: bytes as libc::rlim_t,
    };
    // SAFETY: estrutura inicializada, função altera apenas os limites do worker.
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &limit) } != 0 {
        return Err(());
    }
    // Um abort não deixa core dump com conteúdo do arquivo do cliente.
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) } != 0 {
        return Err(());
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_memory_limit(_: u64) -> Result<(), ()> {
    Ok(())
}

fn encode_error(error: ConvertError) -> [u8; 7] {
    let (tag, row, col) = match error {
        ConvertError::EmptyInput => (1, 0, 0),
        ConvertError::UnsupportedFormat => (2, 0, 0),
        ConvertError::InvalidWorkbook => (3, 0, 0),
        ConvertError::NoSheet => (4, 0, 0),
        ConvertError::TooLarge(reason) => (
            5,
            LIMIT_REASONS.iter().position(|r| *r == reason).unwrap_or(0) as u32,
            0,
        ),
        ConvertError::IllegalCharacter { row, col } => (6, row, col),
        ConvertError::StringTooLong { row, col } => (7, row, col),
        ConvertError::Write => (8, 0, 0),
        ConvertError::DeadlineExceeded => (9, 0, 0),
    };
    let mut header = [0u8; 7];
    header[0] = tag;
    header[1..5].copy_from_slice(&row.to_le_bytes());
    header[5..7].copy_from_slice(&col.to_le_bytes());
    header
}

fn decode_error(header: [u8; 7]) -> ConvertError {
    let row = u32::from_le_bytes(header[1..5].try_into().unwrap());
    let col = u16::from_le_bytes(header[5..7].try_into().unwrap());
    match header[0] {
        1 => ConvertError::EmptyInput,
        2 => ConvertError::UnsupportedFormat,
        3 => ConvertError::InvalidWorkbook,
        4 => ConvertError::NoSheet,
        5 => ConvertError::TooLarge(
            LIMIT_REASONS
                .get(row as usize)
                .copied()
                .unwrap_or("recursos"),
        ),
        6 => ConvertError::IllegalCharacter { row, col },
        7 => ConvertError::StringTooLong { row, col },
        9 => ConvertError::DeadlineExceeded,
        _ => ConvertError::Write,
    }
}
