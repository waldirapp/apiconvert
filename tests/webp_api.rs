//! Rota `POST /webp` nos dois modos: token, corpo multipart ou direto,
//! parâmetros e erros.

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use base64::Engine;
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use tower::ServiceExt;
use xlsconvert::config::Config;

const TOKEN: &str = "token-de-teste-com-mais-de-32-caracteres";
const BOUNDARY: &str = "----delphi-boundary";

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
    app(&[
        ("XLSCONVERT_MODE", "apiconvert"),
        ("XLSCONVERT_WEBP_TOKEN", TOKEN),
    ])
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
        Rgb([(x % 256) as u8, (y % 256) as u8, 90])
    }));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

fn multipart(content: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    b.extend_from_slice(
        b"Content-Disposition: form-data; name=\"foto.png\"; filename=\"foto.png\"\r\n",
    );
    b.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
    b.extend_from_slice(content);
    b.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    b
}

fn post(
    uri: &str,
    token: Option<&str>,
    multipart_body: Option<Vec<u8>>,
    raw: &[u8],
) -> Request<Body> {
    let mut r = Request::post(uri);
    if let Some(t) = token {
        r = r.header("x-internal-token", t);
    }
    match multipart_body {
        Some(b) => r
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(b))
            .unwrap(),
        None => r
            .header("content-type", "application/octet-stream")
            .body(Body::from(raw.to_vec()))
            .unwrap(),
    }
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

fn webp_dims_b64(body: &[u8]) -> (u32, u32) {
    assert!(
        !body.contains(&b'\n') && !body.contains(&b'\r'),
        "base64 sem quebra de linha"
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body)
        .expect("base64 padrão");
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    let img = image::load_from_memory_with_format(&bytes, ImageFormat::WebP).unwrap();
    (img.width(), img.height())
}

#[tokio::test]
async fn multipart_devolve_webp_em_base64_com_lado_maior_400() {
    let app = apiconvert();
    let (st, h, body) = send(
        &app,
        post("/webp", Some(TOKEN), Some(multipart(&png(800, 600))), b""),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h.get("content-type").unwrap(), "text/plain; charset=utf-8");
    assert_eq!(h.get("cache-control").unwrap(), "no-store");
    assert_eq!(webp_dims_b64(&body), (400, 300));
}

#[tokio::test]
async fn corpo_direto_e_parametros() {
    let app = apiconvert();
    let (st, _, body) = send(&app, post("/webp", Some(TOKEN), None, &png(800, 600))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(webp_dims_b64(&body), (400, 300));

    let (st, _, body) = send(
        &app,
        post("/webp?max=200&q=80", Some(TOKEN), None, &png(800, 600)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(webp_dims_b64(&body), (200, 150));

    let (st, _, body) = send(&app, post("/webp?max=0", Some(TOKEN), None, &png(321, 123))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(webp_dims_b64(&body), (321, 123));
}

#[tokio::test]
async fn sem_token_ou_token_errado_401() {
    let app = apiconvert();
    for t in [
        None,
        Some("errado"),
        Some("token-de-teste-com-mais-de-32-caracterex"),
    ] {
        let (st, _, body) = send(&app, post("/webp", t, None, &png(10, 10))).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{t:?}");
        assert!(String::from_utf8_lossy(&body).contains("nao_autorizado"));
    }
}

#[tokio::test]
async fn apiconvert_sem_token_configurado_nao_tem_a_rota() {
    let app = app(&[("XLSCONVERT_MODE", "apiconvert")]);
    let (st, h, body) = send(&app, post("/webp", Some(TOKEN), None, &png(10, 10))).await;
    // Mesmo 404 do legado para rota inexistente.
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(h.get("connection").unwrap(), "close");
    assert_eq!(body, b"Not Found");
}

#[tokio::test]
async fn modo_interno_usa_o_token_interno() {
    let app = app(&[("XLSCONVERT_TOKEN", TOKEN)]);
    let (st, _, body) = send(&app, post("/webp", Some(TOKEN), None, &png(600, 800))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(webp_dims_b64(&body), (300, 400));
}

#[tokio::test]
async fn erros_classificados() {
    let app = apiconvert();
    let casos: Vec<(Request<Body>, StatusCode, &str)> = vec![
        (
            post("/webp", Some(TOKEN), None, b""),
            StatusCode::BAD_REQUEST,
            "imagem_vazia",
        ),
        (
            post("/webp", Some(TOKEN), Some(multipart(b"")), b""),
            StatusCode::BAD_REQUEST,
            "imagem_vazia",
        ),
        (
            post("/webp", Some(TOKEN), None, b"nao sou imagem"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "formato_nao_suportado",
        ),
        (
            post("/webp", Some(TOKEN), None, &png(50, 50)[..70]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "imagem_invalida",
        ),
        (
            post("/webp?max=abc", Some(TOKEN), None, &png(10, 10)),
            StatusCode::BAD_REQUEST,
            "parametro_invalido",
        ),
        (
            post("/webp?max=5000", Some(TOKEN), None, &png(10, 10)),
            StatusCode::BAD_REQUEST,
            "parametro_invalido",
        ),
        (
            post("/webp?q=150", Some(TOKEN), None, &png(10, 10)),
            StatusCode::BAD_REQUEST,
            "parametro_invalido",
        ),
        (
            post("/webp?q=NaN", Some(TOKEN), None, &png(10, 10)),
            StatusCode::BAD_REQUEST,
            "parametro_invalido",
        ),
    ];
    for (i, (req, status, code)) in casos.into_iter().enumerate() {
        let (st, h, body) = send(&app, req).await;
        assert_eq!(st, status, "caso {i}");
        assert_eq!(
            h.get("content-type").unwrap(),
            "application/json; charset=utf-8"
        );
        assert!(String::from_utf8_lossy(&body).contains(code), "caso {i}");
    }
}

#[tokio::test]
async fn imagem_acima_do_teto_de_pixels_413() {
    let app = app(&[
        ("XLSCONVERT_MODE", "apiconvert"),
        ("XLSCONVERT_WEBP_TOKEN", TOKEN),
        ("XLSCONVERT_WEBP_MAX_PIXELS", "10000"),
    ]);
    let (st, _, body) = send(&app, post("/webp", Some(TOKEN), None, &png(200, 200))).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(String::from_utf8_lossy(&body).contains("imagem_grande_demais"));
}

#[tokio::test]
async fn rota_de_planilha_continua_igual_com_a_rota_webp() {
    let app = apiconvert();
    let (st, _, body) = send(
        &app,
        Request::get("/convert?file=nada.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body, b"<HTML><BODY><B>404 Not Found</B></BODY></HTML>");
    let (st, _, _) = send(&app, Request::get("/webp").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
}
