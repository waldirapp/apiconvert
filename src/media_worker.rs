//! Imagens e prévias isoladas no mesmo executável, antes de Tokio e do bind.
//! Pipes limitados; credenciais não são herdadas. Cancelamento encerra e
//! aguarda o filho antes de liberar a vaga de conversão.
use crate::{
    config::Config,
    webp::{self, WebpError, WebpParams},
};
use axum::body::Bytes;
use calamine::Reader;
use serde::{Deserialize, Serialize};
use std::{
    io::{Cursor, Read, Write},
    process::Stdio,
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{oneshot, OwnedSemaphorePermit},
};

pub(crate) type Preview = (usize, usize, Vec<Vec<String>>, bool, usize);
#[derive(Serialize, Deserialize)]
pub(crate) struct ImageInfo {
    pub original: Option<(u32, u32)>,
    pub dimensions: Option<(u32, u32)>,
    pub transparent: bool,
}
pub(crate) struct ImageOutput {
    pub info: ImageInfo,
    pub bytes: Vec<u8>,
}
#[derive(Debug)]
pub(crate) enum WorkerError {
    Timeout,
    Failed,
    TooLarge,
    Image(WebpError),
}

#[derive(Clone, Copy)]
enum Operation {
    Image(WebpParams, u64),
    Preview,
}

pub(crate) async fn image(
    bytes: Bytes,
    params: WebpParams,
    cfg: &Config,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<ImageOutput, WorkerError> {
    let payload = run(
        bytes,
        Operation::Image(params, cfg.webp_max_pixels),
        cfg,
        permit,
    )
    .await?;
    let prefix: [u8; 4] = payload
        .get(..4)
        .ok_or(WorkerError::Failed)?
        .try_into()
        .map_err(|_| WorkerError::Failed)?;
    let len = u32::from_le_bytes(prefix) as usize;
    let end = 4usize.checked_add(len).ok_or(WorkerError::Failed)?;
    let info = serde_json::from_slice(payload.get(4..end).ok_or(WorkerError::Failed)?)
        .map_err(|_| WorkerError::Failed)?;
    Ok(ImageOutput {
        info,
        bytes: payload.get(end..).ok_or(WorkerError::Failed)?.to_vec(),
    })
}

pub(crate) async fn preview(
    bytes: Bytes,
    cfg: &Config,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<Preview, WorkerError> {
    let payload = run(bytes, Operation::Preview, cfg, permit).await?;
    serde_json::from_slice(&payload).map_err(|_| WorkerError::Failed)
}

async fn run(
    bytes: Bytes,
    op: Operation,
    cfg: &Config,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<Vec<u8>, WorkerError> {
    // Config::from_lookup permite o harness embutido dos testes existentes.
    // Config::from_env sempre define worker_path no servidor publicado.
    let Some(path) = cfg.worker_path.clone() else {
        let job = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            execute(&bytes, op)
        });
        return tokio::time::timeout(cfg.timeout, job)
            .await
            .map_err(|_| WorkerError::Timeout)?
            .map_err(|_| WorkerError::Failed)?;
    };
    let timeout = cfg.timeout;
    let max_input = match op {
        Operation::Image(..) => cfg.max_body_bytes,
        Operation::Preview => cfg.compat.max_bytes,
    };
    let max_output = cfg.compat.max_bytes;
    let memory = cfg.media_worker_memory_bytes;
    let (mode, pixels, side, quality) = match op {
        Operation::Image(p, pixels) => ("image", pixels, p.max_side, p.quality),
        Operation::Preview => ("preview", 0, 0, 0.0),
    };
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    // O sender fica na future chamadora. Ao descartá-la (desconexão ou prazo
    // total da tela), o receiver acorda e a tarefa termina o processo filho.
    let job = tokio::spawn(async move {
        let _permit = permit;
        let mut child = tokio::process::Command::new(path)
            .args([
                "--media-worker",
                mode,
                &memory.to_string(),
                &max_input.to_string(),
                &max_output.to_string(),
                &pixels.to_string(),
                &side.to_string(),
                &quality.to_string(),
            ])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| WorkerError::Failed)?;
        let operation = async {
            let mut stdin = child.stdin.take().ok_or(WorkerError::Failed)?;
            stdin
                .write_all(&bytes)
                .await
                .map_err(|_| WorkerError::Failed)?;
            drop(stdin);
            let stdout = child.stdout.take().ok_or(WorkerError::Failed)?;
            let mut output = Vec::new();
            stdout
                .take((max_output as u64).saturating_add(2))
                .read_to_end(&mut output)
                .await
                .map_err(|_| WorkerError::Failed)?;
            if output.len() > max_output.saturating_add(1) {
                return Err(WorkerError::TooLarge);
            }
            let status = child.wait().await.map_err(|_| WorkerError::Failed)?;
            if !status.success() {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if status.signal().is_some() {
                        return Err(WorkerError::TooLarge);
                    }
                }
                return Err(WorkerError::Failed);
            }
            match output.first() {
                Some(0) => Ok(output[1..].to_vec()),
                Some(tag) => Err(decode_error(*tag)),
                None => Err(WorkerError::Failed),
            }
        };
        let result = tokio::select! {
            result = tokio::time::timeout(timeout, operation) => result.unwrap_or(Err(WorkerError::Timeout)),
            _ = &mut cancel_rx => Err(WorkerError::Timeout),
        };
        if result.is_err() {
            // kill() também faz wait: nenhuma vaga reutilizada com filho vivo.
            let _ = child.kill().await;
        }
        result
    });
    let result = job.await.map_err(|_| WorkerError::Failed)?;
    drop(cancel_tx);
    result
}

fn execute(bytes: &[u8], op: Operation) -> Result<Vec<u8>, WorkerError> {
    match op {
        Operation::Preview => {
            serde_json::to_vec(&read_preview(bytes)).map_err(|_| WorkerError::Failed)
        }
        Operation::Image(params, pixels) => {
            let original = webp::dimensions(bytes);
            let out = webp::to_webp(bytes, &params, pixels).map_err(WorkerError::Image)?;
            let info = ImageInfo {
                original,
                dimensions: webp::dimensions(&out),
                transparent: out.len() > 20 && &out[12..16] == b"VP8X" && out[20] & 0x10 != 0,
            };
            let metadata = serde_json::to_vec(&info).map_err(|_| WorkerError::Failed)?;
            let mut payload = Vec::with_capacity(4 + metadata.len() + out.len());
            payload.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
            payload.extend_from_slice(&metadata);
            payload.extend_from_slice(&out);
            Ok(payload)
        }
    }
}

fn read_preview(xlsx: &[u8]) -> Preview {
    let Ok(mut wb) = calamine::Xlsx::new(Cursor::new(xlsx)) else {
        return (0, 0, Vec::new(), false, 0);
    };
    let Some(Ok(range)) = wb.worksheet_range_at(0) else {
        return (0, 0, Vec::new(), false, 0);
    };
    let (rows, cols) = range.get_size();
    let preview_cols = cols.min(20);
    let cells = range
        .rows()
        .take(30)
        .map(|r| {
            r.iter()
                .take(preview_cols)
                .map(|c| match c {
                    calamine::Data::DateTime(d) => d
                        .as_datetime()
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| c.to_string()),
                    _ => c.to_string(),
                })
                .collect()
        })
        .collect();
    (rows, cols, cells, rows > 30 || cols > 20, preview_cols)
}

fn encode_error(e: WorkerError) -> u8 {
    match e {
        WorkerError::Image(WebpError::Empty) => 1,
        WorkerError::Image(WebpError::Unsupported) => 2,
        WorkerError::Image(WebpError::Invalid) => 3,
        WorkerError::TooLarge | WorkerError::Image(WebpError::TooLarge) => 4,
        WorkerError::Image(WebpError::Param("max")) => 5,
        WorkerError::Image(WebpError::Param(_)) => 6,
        WorkerError::Image(WebpError::Encode) => 7,
        _ => 8,
    }
}
fn decode_error(tag: u8) -> WorkerError {
    match tag {
        1 => WorkerError::Image(WebpError::Empty),
        2 => WorkerError::Image(WebpError::Unsupported),
        3 => WorkerError::Image(WebpError::Invalid),
        4 => WorkerError::TooLarge,
        5 => WorkerError::Image(WebpError::Param("max")),
        6 => WorkerError::Image(WebpError::Param("q")),
        7 => WorkerError::Image(WebpError::Encode),
        _ => WorkerError::Failed,
    }
}

/// Despacho antes da configuração do servidor; stdin/stdout são o único IPC.
pub fn worker_entry() -> Option<i32> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--media-worker") {
        return None;
    }
    let run = || -> Result<(), ()> {
        if args.len() != 9 {
            return Err(());
        }
        let memory = args[3].parse().map_err(|_| ())?;
        let max_input: u64 = args[4].parse().map_err(|_| ())?;
        let max_output: usize = args[5].parse().map_err(|_| ())?;
        crate::conversion_worker::set_memory_limit(memory)?;
        let op = match args[2].as_str() {
            "preview" => Operation::Preview,
            "image" => Operation::Image(
                WebpParams {
                    max_side: args[7].parse().map_err(|_| ())?,
                    quality: args[8].parse().map_err(|_| ())?,
                },
                args[6].parse().map_err(|_| ())?,
            ),
            _ => return Err(()),
        };
        let mut input = Vec::new();
        std::io::stdin()
            .take(max_input.checked_add(1).ok_or(())?)
            .read_to_end(&mut input)
            .map_err(|_| ())?;
        let result = if input.len() as u64 > max_input {
            Err(WorkerError::TooLarge)
        } else {
            execute(&input, op)
        };
        let mut stdout = std::io::stdout().lock();
        match result {
            Ok(payload) if payload.len() <= max_output => {
                stdout.write_all(&[0]).map_err(|_| ())?;
                stdout.write_all(&payload).map_err(|_| ())?;
            }
            Ok(_) => stdout.write_all(&[4]).map_err(|_| ())?,
            Err(e) => stdout.write_all(&[encode_error(e)]).map_err(|_| ())?,
        }
        stdout.flush().map_err(|_| ())
    };
    Some(if run().is_ok() { 0 } else { 2 })
}
