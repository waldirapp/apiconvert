//! Telas web: páginas, conversões pela tela, download por id, erros,
//! limite por IP e cabeçalhos de segurança.

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use tower::ServiceExt;
use xlsconvert::config::Config;

const XLS: &[u8] = include_bytes!("fixtures/tipos.xls");
const BOUNDARY: &str = "----navegador";

fn app(pairs: &[(&str, &str)]) -> Router {
    let owned: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let cfg =
        Config::from_lookup(move |k| owned.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()))
            .unwrap();
    xlsconvert::router(cfg)
}

fn apiconvert() -> Router {
    app(&[("XLSCONVERT_MODE", "apiconvert")])
}

/// Multipart como o navegador envia: arquivo em `arquivo` e campos de texto.
fn form(filename: &str, content: &[u8], campos: &[(&str, &str)]) -> Request<Body> {
    let mut b = Vec::new();
    for (k, v) in campos {
        b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        b.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    }
    b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    b.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"arquivo\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    b.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    b.extend_from_slice(content);
    b.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    Request::post("/")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(b))
        .unwrap()
}

fn para(mut req: Request<Body>, uri: &str) -> Request<Body> {
    *req.uri_mut() = uri.parse().unwrap();
    req
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

fn texto(b: &[u8]) -> String {
    String::from_utf8(b.to_vec()).unwrap()
}

/// Id do link `/ui/baixar/<id>` no fragmento.
fn id_download(html: &str) -> String {
    let i = html.find("/ui/baixar/").expect("link de download") + "/ui/baixar/".len();
    html[i..i + 32].to_string()
}

fn png_transparente() -> Vec<u8> {
    let img = DynamicImage::ImageRgba8(RgbaImage::from_fn(80, 40, |x, _| {
        Rgba([200, 30, 30, if x < 40 { 0 } else { 255 }])
    }));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

#[tokio::test]
async fn paginas_com_charset_csp_e_formulario_htmx() {
    let app = apiconvert();
    for (uri, alvo) in [
        ("/", "/planilha"),
        ("/planilha", "/ui/planilha"),
        ("/imagem", "/ui/imagem"),
    ] {
        let (st, h, body) = send(&app, get(uri)).await;
        assert_eq!(st, StatusCode::OK, "{uri}");
        assert_eq!(h.get("content-type").unwrap(), "text/html; charset=utf-8");
        let csp = h.get("content-security-policy").unwrap().to_str().unwrap();
        assert!(csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"));
        assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
        let html = texto(&body);
        assert!(html.contains(r#"<meta charset="utf-8">"#));
        assert!(html.contains(alvo), "{uri}");
        assert!(html.contains("/static/htmx.min.js"));
    }
}

#[tokio::test]
async fn arquivos_estaticos_embutidos() {
    let app = apiconvert();
    for (f, mime) in [
        ("htmx.min.js", "text/javascript; charset=utf-8"),
        ("app.js", "text/javascript; charset=utf-8"),
        ("app.css", "text/css; charset=utf-8"),
    ] {
        let (st, h, body) = send(&app, get(&format!("/static/{f}"))).await;
        assert_eq!(st, StatusCode::OK, "{f}");
        assert_eq!(h.get("content-type").unwrap(), mime);
        assert!(!body.is_empty());
    }
    let (st, _, _) = send(&app, get("/static/../Cargo.toml")).await;
    assert_ne!(st, StatusCode::OK);
    let (st, _, _) = send(&app, get("/static/outro.js")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn planilha_pela_tela_mostra_previa_e_baixa_pelo_id() {
    let app = apiconvert();
    let (st, h, body) = send(
        &app,
        para(form("Relatório NF.xls", XLS, &[]), "/ui/planilha"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h.get("cache-control").unwrap(), "no-store");
    let html = texto(&body);
    assert!(html.contains("Relatório NF.xlsx"));
    assert!(html.contains("<table>"));
    // Data na prévia como data, não como número serial.
    assert!(
        html.contains("2026-10-10 00:00:00") || html.contains("-") && !html.contains(">46305<"),
        "{html}"
    );
    let id = id_download(&html);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));

    let (st, h, xlsx) = send(&app, get(&format!("/ui/baixar/{id}"))).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xlsx.starts_with(b"PK\x03\x04"));
    let cd = h.get("content-disposition").unwrap().to_str().unwrap();
    assert!(
        cd.starts_with("attachment; filename=\"Relat_rio NF.xlsx\""),
        "{cd}"
    );
    assert!(
        cd.contains("filename*=UTF-8''Relat%C3%B3rio%20NF.xlsx"),
        "{cd}"
    );

    // O resultado da tela não aparece no GET do contrato legado.
    let (st, _, _) = send(&app, get("/convert?file=Relat%C3%B3rio%20NF.xlsx")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn imagem_pela_tela_mostra_webp_base64_e_transparencia() {
    let app = apiconvert();
    let (st, _, body) = send(
        &app,
        para(
            form(
                "logo.png",
                &png_transparente(),
                &[("max", "200"), ("q", "50")],
            ),
            "/ui/imagem",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let html = texto(&body);
    assert!(html.contains("data:image/webp;base64,UklGR"));
    assert!(html.contains("80×40"));
    assert!(html.contains("200×100"));
    assert!(html.contains("sim (método 4)"));
    assert!(html.contains("logo.webp"));

    let (st, h, webp) = send(&app, get(&format!("/ui/baixar/{}", id_download(&html)))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h.get("content-type").unwrap(), "image/webp");
    assert_eq!(&webp[8..12], b"WEBP");
}

#[tokio::test]
async fn imagem_pela_tela_nao_exige_token_mas_a_rota_webp_sim() {
    let app = app(&[
        ("XLSCONVERT_MODE", "apiconvert"),
        (
            "XLSCONVERT_WEBP_TOKEN",
            "token-de-teste-com-mais-de-32-caracteres",
        ),
    ]);
    let (st, _, _) = send(
        &app,
        para(form("a.png", &png_transparente(), &[]), "/ui/imagem"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, _) = send(
        &app,
        Request::post("/webp")
            .body(Body::from(png_transparente()))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn erros_viram_fragmento_legivel() {
    let app = apiconvert();
    let casos = [
        (
            para(form("x.xls", b"nao sou planilha", &[]), "/ui/planilha"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Formato não suportado",
        ),
        (
            para(form("x.png", b"nao sou imagem", &[]), "/ui/imagem"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Formato de imagem não suportado",
        ),
        (
            para(form("x.png", b"", &[]), "/ui/imagem"),
            StatusCode::BAD_REQUEST,
            "Escolha um arquivo",
        ),
        (
            para(
                form("x.png", &png_transparente(), &[("max", "abc")]),
                "/ui/imagem",
            ),
            StatusCode::BAD_REQUEST,
            "Lado maior inválido",
        ),
        (
            para(
                form("x.png", &png_transparente(), &[("max", "9999")]),
                "/ui/imagem",
            ),
            StatusCode::BAD_REQUEST,
            "Parâmetro inválido",
        ),
        (
            para(
                form("x.png", &png_transparente(), &[("q", "NaN")]),
                "/ui/imagem",
            ),
            StatusCode::BAD_REQUEST,
            "Qualidade inválida",
        ),
    ];
    for (i, (req, status, msg)) in casos.into_iter().enumerate() {
        let (st, h, body) = send(&app, req).await;
        assert_eq!(st, status, "caso {i}");
        assert_eq!(h.get("content-type").unwrap(), "text/html; charset=utf-8");
        let html = texto(&body);
        assert!(html.contains("role=\"alert\""), "caso {i}");
        assert!(html.contains(msg), "caso {i}: {html}");
    }
    let (st, _, body) = send(&app, get("/ui/baixar/00000000000000000000000000000000")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(texto(&body).contains("expirado"));
}

#[tokio::test]
async fn nome_do_arquivo_e_escapado_no_html() {
    let app = apiconvert();
    let (st, _, body) = send(
        &app,
        para(
            form("<img src=x onerror=alert(1)>.png", &png_transparente(), &[]),
            "/ui/imagem",
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let html = texto(&body);
    assert!(!html.contains("<img src=x"));
    assert!(html.contains("&#60;img src=x") || html.contains("&lt;img src=x"));
}

#[tokio::test]
async fn limite_por_ip() {
    let app = app(&[
        ("XLSCONVERT_MODE", "apiconvert"),
        ("XLSCONVERT_UI_RATE_PER_MIN", "2"),
    ]);
    for _ in 0..2 {
        let (st, _, _) = send(
            &app,
            para(form("a.png", &png_transparente(), &[]), "/ui/imagem"),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }
    let (st, _, body) = send(
        &app,
        para(form("a.png", &png_transparente(), &[]), "/ui/imagem"),
    )
    .await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    assert!(texto(&body).contains("Aguarde um minuto"));
    // As páginas não contam no limite.
    let (st, _, _) = send(&app, get("/imagem")).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn telas_desligadas_e_padrao_do_modo_interno() {
    let desligada = app(&[("XLSCONVERT_MODE", "apiconvert"), ("XLSCONVERT_UI", "0")]);
    let (st, _, body) = send(&desligada, get("/planilha")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body, b"Not Found");

    let interno = app(&[(
        "XLSCONVERT_TOKEN",
        "token-de-teste-com-mais-de-32-caracteres",
    )]);
    let (st, _, _) = send(&interno, get("/")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
