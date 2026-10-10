//! Rotas HTTP. O serviço é interno: só o serviço chamador da mesma máquina o chama,
//! com o header `X-Internal-Token`. A autenticação do usuário e o tenant ficam
//! no serviço chamador.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::convert::{self, ConvertError};

pub const TOKEN_HEADER: &str = "x-internal-token";
pub const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    permits: Arc<Semaphore>,
}

pub fn router(cfg: Config) -> Router {
    let max_body = cfg.max_body_bytes;
    let permits = Arc::new(Semaphore::new(cfg.max_concurrent));
    let imagem = crate::imagem_router(&cfg, permits.clone());
    let ui = crate::ui_router(&cfg, permits.clone());
    let state = AppState {
        permits,
        cfg: Arc::new(cfg),
    };
    let router = Router::new()
        .route("/health", get(health))
        .route("/convert", post(convert_handler))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state);
    [imagem, ui]
        .into_iter()
        .flatten()
        .fold(router, |acc, r| acc.merge(r))
}

async fn health() -> Response {
    json_body(&[("status", "ok")])
}

pub(crate) fn token_ok(headers: &HeaderMap, expected: &str) -> bool {
    let Some(got) = headers.get(TOKEN_HEADER) else {
        return false;
    };
    let got = got.as_bytes();
    let expected = expected.as_bytes();
    // Comparação em tempo constante; tamanhos diferentes já recusam.
    got.len() == expected.len() && bool::from(got.ct_eq(expected))
}

async fn convert_handler(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !token_ok(&headers, &st.cfg.token) {
        return error(
            StatusCode::UNAUTHORIZED,
            "nao_autorizado",
            "token interno ausente ou inválido",
        );
    }
    let Ok(permit) = st.permits.clone().try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ocupado",
            "conversões simultâneas no limite; tente de novo",
        );
    };

    let started = Instant::now();
    let size = body.len();
    let limits = st.cfg.limits.with_timeout(st.cfg.timeout);
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        convert::convert(&body, limits)
    });
    let outcome = tokio::time::timeout(st.cfg.timeout, job).await;
    let elapsed_ms = started.elapsed().as_millis();

    match outcome {
        Err(_) => {
            tracing::warn!(bytes = size, elapsed_ms, "conversão excedeu o tempo limite");
            error(
                StatusCode::GATEWAY_TIMEOUT,
                "tempo_esgotado",
                "conversão excedeu o tempo limite",
            )
        }
        Ok(Err(_)) => {
            tracing::error!(bytes = size, elapsed_ms, "falha interna na conversão");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "erro_interno",
                "falha interna na conversão",
            )
        }
        Ok(Ok(Err(e))) => {
            let (status, code) = classify(&e);
            tracing::info!(
                bytes = size,
                elapsed_ms,
                codigo = code,
                "conversão recusada"
            );
            error(status, code, &e.to_string())
        }
        Ok(Ok(Ok(xlsx))) => {
            tracing::info!(
                bytes = size,
                saida = xlsx.len(),
                elapsed_ms,
                "conversão concluída"
            );
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, XLSX_MIME),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                xlsx,
            )
                .into_response()
        }
    }
}

fn classify(e: &ConvertError) -> (StatusCode, &'static str) {
    match e {
        ConvertError::EmptyInput => (StatusCode::BAD_REQUEST, "arquivo_vazio"),
        ConvertError::UnsupportedFormat => {
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "formato_nao_suportado")
        }
        ConvertError::InvalidWorkbook | ConvertError::NoSheet => {
            (StatusCode::UNPROCESSABLE_ENTITY, "planilha_invalida")
        }
        ConvertError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "planilha_grande_demais"),
        ConvertError::IllegalCharacter { .. } | ConvertError::StringTooLong { .. } => {
            (StatusCode::UNPROCESSABLE_ENTITY, "conteudo_invalido")
        }
        ConvertError::Write => (StatusCode::INTERNAL_SERVER_ERROR, "erro_interno"),
        ConvertError::DeadlineExceeded => (StatusCode::GATEWAY_TIMEOUT, "tempo_esgotado"),
    }
}

pub(crate) fn error(status: StatusCode, code: &str, msg: &str) -> Response {
    let mut resp = json_body(&[("erro", msg), ("codigo", code)]).into_response();
    *resp.status_mut() = status;
    resp
}

/// Monta um objeto JSON plano de strings, com escape correto.
pub(crate) fn json_body(pairs: &[(&str, &str)]) -> Response {
    let mut out = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_str(&mut out, k);
        out.push(':');
        push_json_str(&mut out, v);
    }
    out.push('}');
    (
        [
            (header::CONTENT_TYPE, "application/json; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        out,
    )
        .into_response()
}

fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
