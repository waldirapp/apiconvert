//! Contrato do modo `apiconvert`, caso a caso, contra as respostas capturadas
//! do conversor legado (status, `Content-Type`, corpo, `Content-Disposition`
//! e `Connection`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use tower::ServiceExt;
use xlsconvert::config::Config;

const XLS: &[u8] = include_bytes!("fixtures/tipos.xls");
const XLSX_MIME: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const BOUNDARY: &str = "----delphi-boundary";

#[tokio::test]
async fn head_consulta_metadados_sem_consumir_o_download() {
    let app = app();
    let (status, _, _) = send(&app, post_multipart(multipart("f", Some(b"head.xls"), XLS))).await;
    assert_eq!(status, StatusCode::OK);
    for _ in 0..2 {
        let (status, headers, body) = send(
            &app,
            Request::head("/convert?file=head.xlsx")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(header(&headers, "content-type"), XLSX_MIME.as_bytes());
        assert_eq!(
            header(&headers, "content-disposition"),
            b"attachment; filename=\"head.xlsx\""
        );
        assert!(body.is_empty());
    }
    let (status, _, body) = send(
        &app,
        Request::get("/convert?file=head.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with(b"PK"));
    let (status, _, _) = send(
        &app,
        Request::get("/convert?file=head.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn app() -> Router {
    let pairs = [("XLSCONVERT_MODE".to_string(), "apiconvert".to_string())];
    let cfg =
        Config::from_lookup(move |k| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()))
            .unwrap();
    xlsconvert::router(cfg)
}

/// Multipart como o `TMultipartFormData.AddFile` do Delphi: campo com o nome
/// do arquivo.
fn multipart(field: &str, filename: Option<&[u8]>, content: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    b.extend_from_slice(format!("Content-Disposition: form-data; name=\"{field}\"").as_bytes());
    if let Some(f) = filename {
        b.extend_from_slice(b"; filename=\"");
        b.extend_from_slice(f);
        b.extend_from_slice(b"\"");
    }
    b.extend_from_slice(b"\r\nContent-Type: application/vnd.ms-excel\r\n\r\n");
    b.extend_from_slice(content);
    b.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    b
}

fn post_multipart(body: Vec<u8>) -> Request<Body> {
    Request::post("/convert")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap()
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

fn header<'a>(h: &'a axum::http::HeaderMap, k: &str) -> &'a [u8] {
    h.get(k).map(|v| v.as_bytes()).unwrap_or(b"")
}

#[tokio::test]
async fn post_valido_devolve_nome_e_get_baixa_com_content_disposition() {
    let app = app();
    let (st, h, body) = send(
        &app,
        post_multipart(multipart("tmp33E7.xls", Some(b"tmp33E7.xls"), XLS)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(header(&h, "content-type"), XLSX_MIME.as_bytes());
    assert_eq!(body, b"tmp33E7.xlsx");

    let (st, h, body) = send(
        &app,
        Request::get("/convert?file=tmp33E7.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(header(&h, "content-type"), XLSX_MIME.as_bytes());
    assert_eq!(
        header(&h, "content-disposition"),
        b"attachment; filename=\"tmp33E7.xlsx\""
    );
    assert!(body.starts_with(b"PK\x03\x04"));

    // Divergência intencional (GHSA-jc42-3ccg-wf85): o legado permitia baixar
    // de novo; aqui o resultado é entregue uma única vez. O cliente baixa uma
    // vez só, logo depois do POST (dUploadXlsApi.UploadXlsApi).
    let (st, _, body) = send(
        &app,
        Request::get("/convert?file=tmp33E7.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body, b"<HTML><BODY><B>404 Not Found</B></BODY></HTML>");
}

#[tokio::test]
async fn nome_vem_do_filename_com_a_ultima_extensao_trocada() {
    let app = app();
    for (enviado, esperado) in [
        (&b"semext"[..], &b"semext.xlsx"[..]),
        (b"captura.dois.XLS", b"captura.dois.xlsx"),
        (
            b"C:\\Users\\x\\AppData\\Local\\Temp\\tmpA1.tmp",
            b"tmpA1.xlsx",
        ),
    ] {
        let (st, _, body) = send(
            &app,
            post_multipart(multipart("qualquer", Some(enviado), XLS)),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body, esperado);
    }
}

#[tokio::test]
async fn nome_com_acento_volta_em_latin1_e_o_get_funciona() {
    let app = app();
    let nome = "NF recebidas ação.xls".as_bytes();
    let (st, _, body) = send(&app, post_multipart(multipart("f", Some(nome), XLS))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"NF recebidas a\xe7\xe3o.xlsx".to_vec());

    // GET com o nome em UTF-8 percent-encoded (o que um cliente HTTP envia)
    let (st, h, _) = send(
        &app,
        Request::get("/convert?file=NF%20recebidas%20a%C3%A7%C3%A3o.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        header(&h, "content-disposition"),
        b"attachment; filename=\"NF recebidas a\xe7\xe3o.xlsx\"".to_vec()
    );
    // e em Latin-1 percent-encoded (novo envio: o resultado é de uso único)
    send(&app, post_multipart(multipart("f", Some(nome), XLS))).await;
    let (st, _, _) = send(
        &app,
        Request::get("/convert?file=NF%20recebidas%20a%E7%E3o.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn nome_fora_de_latin1_volta_com_interrogacao_e_o_get_desse_nome_funciona() {
    let app = app();
    let (st, _, body) = send(
        &app,
        post_multipart(multipart("f", Some("relatório€.xls".as_bytes()), XLS)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"relat\xf3rio?.xlsx".to_vec());
    // o cliente usa o nome recebido: Latin-1, com '?' no lugar do '€'
    let (st, _, _) = send(
        &app,
        Request::get("/convert?file=relat%F3rio%3F.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

/// O `.xlsx` tem o texto `t` nas strings compartilhadas.
fn contem_texto(xlsx: &[u8], t: &str) -> bool {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(xlsx)).unwrap();
    let mut xml = String::new();
    match z.by_name("xl/sharedStrings.xml") {
        Ok(mut f) => {
            f.read_to_string(&mut xml).unwrap();
        }
        Err(_) => return false,
    }
    xml.contains(t)
}

#[tokio::test]
async fn alias_nao_substitui_resultado_com_o_mesmo_nome() {
    let app = app();
    // 1º: "a?.xls" devolve "a?.xlsx" (nome primário)
    let (_, _, body) = send(&app, post_multipart(multipart("f", Some(b"a?.xls"), XLS))).await;
    assert_eq!(body, b"a?.xlsx");
    let get = |q: &'static str| Request::get(q).body(Body::empty()).unwrap();

    // 2º: "a€.xls" também devolve "a?.xlsx", com outra planilha
    let mut wb = rust_xlsxwriter::Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "H").unwrap();
    s.write_string(1, 0, "outra").unwrap();
    let outra = wb.save_to_buffer().unwrap();
    let (st, _, body) = send(
        &app,
        post_multipart(multipart("f", Some("a€.xls".as_bytes()), &outra)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, b"a?.xlsx");

    // o primeiro resultado continua sob o nome dele
    let (st, _, primeiro) = send(&app, get("/convert?file=a%3F.xlsx")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!contem_texto(&primeiro, "outra"));
    // e o segundo segue baixável pelo nome primário (UTF-8)
    let (st, _, segundo) = send(&app, get("/convert?file=a%E2%82%AC.xlsx")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(contem_texto(&segundo, "outra"));
}

fn de(ip: [u8; 4], mut req: Request<Body>) -> Request<Body> {
    req.extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            ip, 40000,
        ))));
    req
}

fn get_de(ip: [u8; 4], uri: &str) -> Request<Body> {
    de(ip, Request::get(uri).body(Body::empty()).unwrap())
}

const CLIENTE: [u8; 4] = [200, 1, 1, 10];
const OUTRO: [u8; 4] = [203, 0, 113, 66];

#[tokio::test]
async fn resultado_so_e_entregue_ao_ip_que_enviou() {
    let app = app();
    let (st, _, nome) = send(
        &app,
        de(
            CLIENTE,
            post_multipart(multipart("x", Some(b"tmp1A2B.xls"), XLS)),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(nome, b"tmp1A2B.xlsx");

    // outro IP com o nome certo: 404 do legado, e o resultado não é consumido
    let (st, _, body) = send(&app, get_de(OUTRO, "/convert?file=tmp1A2B.xlsx")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body, b"<HTML><BODY><B>404 Not Found</B></BODY></HTML>");
    // nem sem a informação de IP
    let (st, _, _) = send(
        &app,
        Request::get("/convert?file=tmp1A2B.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, _, xlsx) = send(&app, get_de(CLIENTE, "/convert?file=tmp1A2B.xlsx")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(xlsx.starts_with(b"PK\x03\x04"));
}

#[tokio::test]
async fn mesmo_nome_de_ips_diferentes_nao_se_misturam() {
    let app = app();
    let mut wb = rust_xlsxwriter::Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "H").unwrap();
    s.write_string(1, 0, "outra").unwrap();
    let outra = wb.save_to_buffer().unwrap();
    send(
        &app,
        de(
            CLIENTE,
            post_multipart(multipart("x", Some(b"tmp0001.xls"), XLS)),
        ),
    )
    .await;
    send(
        &app,
        de(
            OUTRO,
            post_multipart(multipart("x", Some(b"tmp0001.xls"), &outra)),
        ),
    )
    .await;

    let (st, _, do_cliente) = send(&app, get_de(CLIENTE, "/convert?file=tmp0001.xlsx")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(!contem_texto(&do_cliente, "outra"));
    let (st, _, do_outro) = send(&app, get_de(OUTRO, "/convert?file=tmp0001.xlsx")).await;
    assert_eq!(st, StatusCode::OK);
    assert!(contem_texto(&do_outro, "outra"));
}

#[tokio::test]
async fn varredura_de_nomes_bloqueia_o_ip() {
    let app = app();
    send(
        &app,
        de(
            CLIENTE,
            post_multipart(multipart("x", Some(b"tmpBEEF.xls"), XLS)),
        ),
    )
    .await;
    // 30 nomes inexistentes no minuto (o padrão) a partir do mesmo IP...
    for i in 0..30 {
        let uri = format!("/convert?file=tmp{i:04X}.xlsx");
        let (st, _, _) = send(&app, get_de(CLIENTE, &uri)).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
    // ...e até o nome certo responde 404 até o fim da janela
    let (st, _, body) = send(&app, get_de(CLIENTE, "/convert?file=tmpBEEF.xlsx")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body, b"<HTML><BODY><B>404 Not Found</B></BODY></HTML>");
    // outro IP não é afetado
    send(
        &app,
        de(
            OUTRO,
            post_multipart(multipart("x", Some(b"tmpCAFE.xls"), XLS)),
        ),
    )
    .await;
    let (st, _, _) = send(&app, get_de(OUTRO, "/convert?file=tmpCAFE.xlsx")).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn protecoes_desligadas_voltam_ao_comportamento_do_legado() {
    let pairs = [
        ("XLSCONVERT_MODE".to_string(), "apiconvert".to_string()),
        ("XLSCONVERT_COMPAT_BIND_IP".to_string(), "0".to_string()),
        ("XLSCONVERT_COMPAT_ONE_TIME".to_string(), "0".to_string()),
    ];
    let cfg =
        Config::from_lookup(move |k| pairs.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()))
            .unwrap();
    let app = xlsconvert::router(cfg);
    send(
        &app,
        de(
            CLIENTE,
            post_multipart(multipart("x", Some(b"tmp9999.xls"), XLS)),
        ),
    )
    .await;
    for ip in [OUTRO, CLIENTE] {
        let (st, _, _) = send(&app, get_de(ip, "/convert?file=tmp9999.xlsx")).await;
        assert_eq!(st, StatusCode::OK);
    }
}

#[tokio::test]
async fn get_inexistente_ou_sem_parametro_404_do_legado() {
    let app = app();
    for uri in ["/convert?file=tmpZZZZ.xlsx", "/convert", "/convert?outro=1"] {
        let (st, h, body) = send(&app, Request::get(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(header(&h, "content-type"), b"text/html; charset=utf-8");
        assert_eq!(header(&h, "connection"), b"close");
        assert_eq!(body, b"<HTML><BODY><B>404 Not Found</B></BODY></HTML>");
    }
}

#[tokio::test]
async fn get_nao_serve_arquivo_que_o_servico_nao_converteu() {
    let app = app();
    // Nome enviado (.xls) e caminhos com diretório nunca viram leitura de disco.
    send(&app, post_multipart(multipart("a", Some(b"a.xls"), XLS))).await;
    for uri in [
        "/convert?file=a.xls",
        "/convert?file=../../etc/passwd",
        "/convert?file=%2Fetc%2Fpasswd",
    ] {
        let (st, _, _) = send(&app, Request::get(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn falhas_no_post_viram_500_erro_ao_gravar() {
    let app = app();
    let casos: Vec<Request<Body>> = vec![
        // arquivo que não é planilha
        post_multipart(multipart("x.xls", Some(b"x.xls"), b"nao sou planilha")),
        // OLE2 corrompido
        post_multipart(multipart("c.xls", Some(b"c.xls"), &{
            let mut d = XLS.to_vec();
            for b in d.iter_mut().take(2400).skip(1600) {
                *b = 0x01;
            }
            d
        })),
        // sem multipart
        Request::post("/convert")
            .body(Body::from(XLS.to_vec()))
            .unwrap(),
        // multipart sem arquivo
        post_multipart(multipart("campo", None, b"valor")),
        // nome com caractere de controle (injetaria no Content-Disposition);
        // aspas não chegam até aqui: o parser multipart encerra o nome nelas.
        post_multipart(multipart("x", Some(b"a\x01b.xls"), XLS)),
    ];
    for (i, req) in casos.into_iter().enumerate() {
        let (st, h, body) = send(&app, req).await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "caso {i}");
        assert_eq!(header(&h, "content-type"), b"text/html; charset=UTF-8");
        assert_eq!(body, b"Erro ao gravar o arquivo");
    }
}

#[tokio::test]
async fn rota_inexistente_e_metodo_nao_permitido() {
    let app = app();
    let (st, h, body) = send(&app, Request::get("/outra").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(header(&h, "content-type"), b"text/html; charset=UTF-8");
    assert_eq!(header(&h, "connection"), b"close");
    assert_eq!(body, b"Not Found");

    let (st, h, body) = send(&app, Request::put("/convert").body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header(&h, "content-type"), b"text/html; charset=UTF-8");
    assert_eq!(body, b"Method Not Allowed");
}

#[tokio::test]
async fn mesmo_nome_substitui_o_anterior() {
    let app = app();
    send(&app, post_multipart(multipart("n", Some(b"n.xls"), XLS))).await;
    let (_, _, primeiro) = send(
        &app,
        Request::get("/convert?file=n.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    // segunda planilha diferente com o mesmo nome
    let mut wb = rust_xlsxwriter::Workbook::new();
    let s = wb.add_worksheet();
    s.write_string(0, 0, "H").unwrap();
    s.write_string(1, 0, "outra").unwrap();
    let outra = wb.save_to_buffer().unwrap();
    send(&app, post_multipart(multipart("n", Some(b"n.xls"), &outra))).await;
    let (_, _, segundo) = send(
        &app,
        Request::get("/convert?file=n.xlsx")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_ne!(primeiro, segundo);
}
