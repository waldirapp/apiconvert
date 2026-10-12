//! Regressões do isolamento; arquivos sintéticos, sem pandas.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use image::{DynamicImage, ImageFormat};
use std::io::Cursor;
use tower::ServiceExt;
use xlsconvert::config::Config;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
fn cfg() -> Config {
    Config::from_lookup(|k| match k {
        "XLSCONVERT_TOKEN" => Some(TOKEN.into()),
        "XLSCONVERT_UI" => Some("1".into()),
        _ => None,
    })
    .unwrap()
}
fn png() -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(10, 10)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}
#[cfg(unix)]
#[tokio::test]
async fn rota_imagem_usa_worker_e_sobrevive_a_falha() {
    let mut cfg = cfg();
    cfg.worker_path = Some("/bin/false".into());
    let app = xlsconvert::router(cfg);
    let response = app
        .clone()
        .oneshot(
            Request::post("/webp")
                .header("x-internal-token", TOKEN)
                .body(Body::from(png()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        app.oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

fn isolated_cfg() -> Config {
    let mut c = cfg();
    c.worker_path = Some(env!("CARGO_BIN_EXE_xlsconvert").into());
    c.max_concurrent = 1;
    c
}

#[tokio::test]
async fn imagem_pela_tela_isolada_preserva_previa_e_download() {
    let mut input = b"--isolated\r\nContent-Disposition: form-data; name=\"arquivo\"; filename=\"foto.png\"\r\n\r\n".to_vec();
    input.extend_from_slice(&png());
    input.extend_from_slice(b"\r\n--isolated--\r\n");
    let app = xlsconvert::router(isolated_cfg());
    let r = app
        .clone()
        .oneshot(
            Request::post("/ui/imagem")
                .header("content-type", "multipart/form-data; boundary=isolated")
                .body(Body::from(input))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let html = String::from_utf8(response_bytes(r).await).unwrap();
    assert!(html.contains("foto.webp"));
    let encoded = html
        .split("data:image/webp;base64,")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    use base64::Engine;
    let webp = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let download = html
        .split("/ui/baixar/")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let r = app
        .oneshot(
            Request::get(format!("/ui/baixar/{download}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(response_bytes(r).await, webp);
}
fn image_request(bytes: Vec<u8>, uri: &str) -> Request<Body> {
    Request::post(uri)
        .header("x-internal-token", TOKEN)
        .body(Body::from(bytes))
        .unwrap()
}
/// XLSX sintético independente do leitor BIFF e das fixtures em disco.
fn synthetic_workbook() -> Vec<u8> {
    let mut workbook = rust_xlsxwriter::Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.write_string(0, 0, "Nome").unwrap();
    sheet.write_string(0, 1, "Data").unwrap();
    sheet.write_string(0, 2, "Valor").unwrap();
    let date_format = rust_xlsxwriter::Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    for row in 1..=40 {
        sheet
            .write_string(row, 0, format!("Produto {row}"))
            .unwrap();
        sheet
            .write_number_with_format(row, 1, 46305.5, &date_format)
            .unwrap();
        sheet.write_number(row, 2, f64::from(row) + 0.25).unwrap();
    }
    workbook.save_to_buffer().unwrap()
}

fn spreadsheet_request() -> Request<Body> {
    let mut body = b"--isolated\r\nContent-Disposition: form-data; name=\"arquivo\"; filename=\"teste.xlsx\"\r\n\r\n".to_vec();
    body.extend_from_slice(&synthetic_workbook());
    body.extend_from_slice(b"\r\n--isolated--\r\n");
    Request::post("/ui/planilha")
        .header("content-type", "multipart/form-data; boundary=isolated")
        .body(Body::from(body))
        .unwrap()
}
async fn response_bytes(response: axum::response::Response) -> Vec<u8> {
    use http_body_util::BodyExt;
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn imagem_isolada_preserva_bytes_base64_e_erros() {
    use base64::Engine;
    use xlsconvert::webp::{to_webp, WebpParams};
    let input = png();
    let expected = to_webp(&input, &WebpParams::default(), 25_000_000).unwrap();
    let app = xlsconvert::router(isolated_cfg());
    let r = app
        .clone()
        .oneshot(image_request(input, "/webp"))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "text/plain; charset=utf-8");
    assert_eq!(r.headers()["cache-control"], "no-store");
    let out = base64::engine::general_purpose::STANDARD
        .decode(response_bytes(r).await)
        .unwrap();
    assert_eq!(out, expected);
    for (bytes, uri, status, code) in [
        (Vec::new(), "/webp", StatusCode::BAD_REQUEST, "imagem_vazia"),
        (
            b"nao imagem".to_vec(),
            "/webp",
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "formato_nao_suportado",
        ),
        (
            png(),
            "/webp?max=4097",
            StatusCode::BAD_REQUEST,
            "parametro_invalido",
        ),
        (
            png(),
            "/webp?q=101",
            StatusCode::BAD_REQUEST,
            "parametro_invalido",
        ),
    ] {
        let r = app
            .clone()
            .oneshot(image_request(bytes, uri))
            .await
            .unwrap();
        assert_eq!(r.status(), status);
        assert!(String::from_utf8(response_bytes(r).await)
            .unwrap()
            .contains(code));
    }
}

#[tokio::test]
async fn previa_isolada_preserva_html_e_download() {
    let app = xlsconvert::router(isolated_cfg());
    let r = app.clone().oneshot(spreadsheet_request()).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let html = String::from_utf8(response_bytes(r).await).unwrap();
    assert!(html.contains("teste.xlsx"));
    assert!(html.contains("<table"));
    let download = html
        .split("/ui/baixar/")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let r = app
        .oneshot(
            Request::get(format!("/ui/baixar/{download}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let bytes = response_bytes(r).await;
    assert!(bytes.starts_with(b"PK"));
    // O conteúdo também mantém o resultado da conversão direta.
    let expected = xlsconvert::convert::convert(&synthetic_workbook(), Default::default()).unwrap();
    use calamine::Reader;
    let mut wb = calamine::Xlsx::new(Cursor::new(bytes)).unwrap();
    let mut reference = calamine::Xlsx::new(Cursor::new(expected)).unwrap();
    assert_eq!(
        wb.worksheet_range_at(0).unwrap().unwrap(),
        reference.worksheet_range_at(0).unwrap().unwrap()
    );
}

#[cfg(target_os = "linux")]
mod unix {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Harness {
        script: PathBuf,
        pid: PathBuf,
    }
    impl Harness {
        fn new(preview_only: bool) -> Self {
            let stem = std::env::temp_dir().join(format!(
                "apiconvert-isolation-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let h = Self {
                script: stem.with_extension("sh"),
                pid: stem.with_extension("pid"),
            };
            let real = env!("CARGO_BIN_EXE_xlsconvert");
            let dispatch = if preview_only {
                format!("if [ \"$1\" != --media-worker ] || [ \"$2\" != preview ]; then exec '{real}' \"$@\"; fi\n")
            } else {
                String::new()
            };
            h.write(&format!(
                "{dispatch}cat >/dev/null\nprintf '%s' \"$$\" > '{}'\nexec sleep 60\n",
                h.pid.display()
            ));
            h
        }
        fn write(&self, body: &str) {
            fs::write(&self.script, format!("#!/bin/sh\n{body}")).unwrap();
            fs::set_permissions(&self.script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        async fn pid(&self) -> u32 {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(value) = fs::read_to_string(&self.pid) {
                        break value.parse().unwrap();
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap()
        }
        async fn assert_reaped(&self, pid: u32) {
            tokio::time::timeout(Duration::from_secs(3), async {
                while PathBuf::from(format!("/proc/{pid}")).exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("filho foi encerrado e coletado");
        }
    }
    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.script);
            let _ = fs::remove_file(&self.pid);
        }
    }

    #[tokio::test]
    async fn timeout_imagem_encerra_filho_e_libera_vaga() {
        let h = Harness::new(false);
        let mut c = isolated_cfg();
        c.worker_path = Some(h.script.clone());
        c.timeout = Duration::from_millis(300);
        let app = xlsconvert::router(c);
        let r = app
            .clone()
            .oneshot(image_request(png(), "/webp"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::GATEWAY_TIMEOUT);
        drop(r);
        h.assert_reaped(h.pid().await).await;
        h.write(&format!(
            "exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_xlsconvert")
        ));
        let r = app.oneshot(image_request(png(), "/webp")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    #[tokio::test]
    async fn cancelamento_imagem_encerra_filho_e_libera_vaga() {
        let h = Harness::new(false);
        let mut c = isolated_cfg();
        c.worker_path = Some(h.script.clone());
        let app = xlsconvert::router(c);
        let a = app.clone();
        let task = tokio::spawn(async move { a.oneshot(image_request(png(), "/webp")).await });
        let pid = h.pid().await;
        task.abort();
        let _ = task.await;
        h.assert_reaped(pid).await;
        h.write(&format!(
            "exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_xlsconvert")
        ));
        assert_eq!(
            app.oneshot(image_request(png(), "/webp"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn timeout_da_previa_encerra_filho_e_permite_nova_planilha() {
        let h = Harness::new(true);
        let mut c = isolated_cfg();
        c.worker_path = Some(h.script.clone());
        c.timeout = Duration::from_secs(2);
        let app = xlsconvert::router(c);
        let r = app.clone().oneshot(spreadsheet_request()).await.unwrap();
        assert_eq!(r.status(), StatusCode::GATEWAY_TIMEOUT);
        drop(r);
        h.assert_reaped(h.pid().await).await;
        h.write(&format!(
            "exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_xlsconvert")
        ));
        assert_eq!(
            app.oneshot(spreadsheet_request()).await.unwrap().status(),
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn memoria_da_imagem_limitada_nao_derruba_servidor() {
        let mut c = isolated_cfg();
        c.media_worker_memory_bytes = 64 * 1024 * 1024;
        let app = xlsconvert::router(c);
        let mut input = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(3000, 3000)
            .write_to(&mut input, ImageFormat::Png)
            .unwrap();
        let r = app
            .clone()
            .oneshot(image_request(input.into_inner(), "/webp"))
            .await
            .unwrap();
        assert!(r.status().is_client_error() || r.status().is_server_error());
        drop(r);
        assert_eq!(
            app.oneshot(image_request(png(), "/webp"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
}
