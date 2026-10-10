//! Rota `POST /webp`: converte uma imagem em WebP e devolve o resultado em
//! base64, como o `ConvertImgToWebp` do x-duall devolve para o campo
//! `imagem1`.
//!
//! | Item | Contrato |
//! |---|---|
//! | Autenticação | Header `X-Internal-Token` (no modo `apiconvert`, `XLSCONVERT_WEBP_TOKEN`; sem ele a rota não existe) |
//! | Corpo | Multipart com o arquivo (primeiro campo com conteúdo) ou os bytes da imagem direto |
//! | Query | `max` (lado maior, padrão 400, 0 mantém o tamanho, até 4096) e `q` (qualidade, padrão 50) |
//! | 200 | `text/plain; charset=utf-8`, WebP em base64 padrão sem quebra de linha |
//! | Erros | JSON `{"erro", "codigo"}`: 400 `imagem_vazia`/`parametro_invalido`, 401 `nao_autorizado`, 413 `imagem_grande_demais`, 415 `formato_nao_suportado`, 422 `imagem_invalida`, 503 `ocupado`, 504 `tempo_esgotado` |

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use base64::Engine;
use tokio::sync::Semaphore;

use crate::app::{error, token_ok};
use crate::webp::{self, WebpError, WebpParams};

#[derive(Clone)]
struct ImagemState {
    token: Arc<str>,
    permits: Arc<Semaphore>,
    timeout: Duration,
    max_pixels: u64,
}

/// Rotas de imagem; `token` é o que o header `X-Internal-Token` precisa trazer.
/// `permits` é o semáforo das conversões de planilha: imagem e planilha
/// dividem o mesmo teto de conversões simultâneas (e de memória).
pub fn router(
    token: &str,
    max_body: usize,
    permits: Arc<Semaphore>,
    timeout: Duration,
    max_pixels: u64,
) -> Router {
    let state = ImagemState {
        token: Arc::from(token),
        permits,
        timeout,
        max_pixels,
    };
    Router::new()
        .route("/webp", post(webp_handler))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state)
}

fn params(uri: &Uri) -> Result<WebpParams, &'static str> {
    let mut p = WebpParams::default();
    for pair in uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|s| !s.is_empty())
    {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "max" => p.max_side = v.parse().map_err(|_| "max")?,
            "q" => {
                p.quality = v.parse().map_err(|_| "q")?;
                if !p.quality.is_finite() {
                    return Err("q");
                }
            }
            _ => {}
        }
    }
    Ok(p)
}

/// Recusa ao ler o corpo: status, código e mensagem.
type Recusa = (StatusCode, &'static str, &'static str);

const CORPO_GRANDE: Recusa = (
    StatusCode::PAYLOAD_TOO_LARGE,
    "imagem_grande_demais",
    "corpo acima do limite",
);
const MULTIPART_INVALIDO: Recusa = (
    StatusCode::BAD_REQUEST,
    "imagem_vazia",
    "multipart inválido",
);

/// Bytes da imagem: o primeiro campo com conteúdo do multipart, ou o corpo.
async fn image_bytes(req: Request) -> Result<Bytes, Recusa> {
    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("multipart/form-data"));
    if !is_multipart {
        return Bytes::from_request(req, &()).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                CORPO_GRANDE
            } else {
                (StatusCode::BAD_REQUEST, "imagem_vazia", "corpo ilegível")
            }
        });
    }
    let mut mp = Multipart::from_request(req, &())
        .await
        .map_err(|_| MULTIPART_INVALIDO)?;
    let por_status = |s: StatusCode| {
        if s == StatusCode::PAYLOAD_TOO_LARGE {
            CORPO_GRANDE
        } else {
            MULTIPART_INVALIDO
        }
    };
    while let Some(field) = mp.next_field().await.map_err(|e| por_status(e.status()))? {
        let data = field.bytes().await.map_err(|e| por_status(e.status()))?;
        if !data.is_empty() {
            return Ok(data);
        }
    }
    Ok(Bytes::new())
}

async fn webp_handler(State(st): State<ImagemState>, req: Request) -> Response {
    if !token_ok(req.headers(), &st.token) {
        return error(
            StatusCode::UNAUTHORIZED,
            "nao_autorizado",
            "token interno ausente ou inválido",
        );
    }
    let params = match params(req.uri()) {
        Ok(p) => p,
        Err(k) => {
            return error(
                StatusCode::BAD_REQUEST,
                "parametro_invalido",
                &format!("parâmetro inválido: {k}"),
            )
        }
    };
    let bytes = match image_bytes(req).await {
        Ok(b) => b,
        Err((status, code, msg)) => return error(status, code, msg),
    };
    let Ok(permit) = st.permits.clone().try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ocupado",
            "conversões simultâneas no limite; tente de novo",
        );
    };

    let started = Instant::now();
    let size = bytes.len();
    let max_pixels = st.max_pixels;
    let job = tokio::task::spawn_blocking(move || {
        // A vaga só é liberada quando a conversão termina, mesmo após 504.
        let _permit = permit;
        webp::to_webp(&bytes, &params, max_pixels)
    });
    let outcome = tokio::time::timeout(st.timeout, job).await;
    let elapsed_ms = started.elapsed().as_millis();

    match outcome {
        Err(_) => {
            tracing::warn!(bytes = size, elapsed_ms, "webp excedeu o tempo limite");
            error(
                StatusCode::GATEWAY_TIMEOUT,
                "tempo_esgotado",
                "conversão excedeu o tempo limite",
            )
        }
        Ok(Err(_)) => {
            tracing::error!(bytes = size, elapsed_ms, "falha interna no webp");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "erro_interno",
                "falha interna na conversão",
            )
        }
        Ok(Ok(Err(e))) => {
            let (status, code) = classify(&e);
            tracing::info!(bytes = size, elapsed_ms, codigo = code, "webp recusado");
            error(status, code, &e.to_string())
        }
        Ok(Ok(Ok(out))) => {
            tracing::info!(
                bytes = size,
                saida = out.len(),
                elapsed_ms,
                "webp concluído"
            );
            let b64 = base64::engine::general_purpose::STANDARD.encode(&out);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                b64,
            )
                .into_response()
        }
    }
}

fn classify(e: &WebpError) -> (StatusCode, &'static str) {
    match e {
        WebpError::Empty => (StatusCode::BAD_REQUEST, "imagem_vazia"),
        WebpError::Param(_) => (StatusCode::BAD_REQUEST, "parametro_invalido"),
        WebpError::Unsupported => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "formato_nao_suportado"),
        WebpError::Invalid => (StatusCode::UNPROCESSABLE_ENTITY, "imagem_invalida"),
        WebpError::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "imagem_grande_demais"),
        WebpError::Encode => (StatusCode::INTERNAL_SERVER_ERROR, "erro_interno"),
    }
}
