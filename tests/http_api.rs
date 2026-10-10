//! Testes das rotas HTTP: token interno obrigatório, limites e respostas.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rust_xlsxwriter::Workbook;
use tower::ServiceExt;
use xlsconvert::app::{router, TOKEN_HEADER, XLSX_MIME};
use xlsconvert::config::Config;

const TOKEN: &str = "token-de-teste-com-mais-de-32-caracteres";

fn cfg(max_body_mb: &str) -> Config {
    let pairs = [
        ("XLSCONVERT_TOKEN".to_string(), TOKEN.to_string()),
        (
            "XLSCONVERT_MAX_BODY_MB".to_string(),
            max_body_mb.to_string(),
        ),
    ];
    Config::from_lookup(move |k| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()))
        .unwrap()
}

fn sample_xlsx() -> Vec<u8> {
    let mut wb = Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "H").unwrap();
    s.write_string(1, 0, "v").unwrap();
    wb.save_to_buffer().unwrap()
}

fn post(body: Vec<u8>, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri("/convert");
    if let Some(t) = token {
        b = b.header(TOKEN_HEADER, t);
    }
    b.body(Body::from(body)).unwrap()
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[tokio::test]
async fn health_sem_token() {
    let resp = router(cfg("20"))
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_string(resp).await, r#"{"status":"ok"}"#);
}

#[tokio::test]
async fn convert_sem_token_ou_com_token_errado_recusa() {
    let resp = router(cfg("20"))
        .oneshot(post(sample_xlsx(), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let errado = "x".repeat(TOKEN.len());
    let resp = router(cfg("20"))
        .oneshot(post(sample_xlsx(), Some(&errado)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(!body_string(resp).await.contains(TOKEN));
}

#[tokio::test]
async fn convert_com_token_devolve_xlsx() {
    let resp = router(cfg("20"))
        .oneshot(post(sample_xlsx(), Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], XLSX_MIME);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"PK\x03\x04"));
}

#[tokio::test]
async fn formato_nao_suportado_e_corpo_vazio() {
    let resp = router(cfg("20"))
        .oneshot(post(b"<html></html>".to_vec(), Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(body_string(resp).await.contains("formato_nao_suportado"));

    let resp = router(cfg("20"))
        .oneshot(post(Vec::new(), Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn corpo_acima_do_limite() {
    let grande = vec![0u8; 2 * 1024 * 1024];
    let resp = router(cfg("1"))
        .oneshot(post(grande, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
