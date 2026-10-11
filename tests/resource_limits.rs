//! Regressões de memória e admissão; todas as entradas são sintéticas.
use std::io::{Cursor, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use futures_util::stream;
use rust_xlsxwriter::Workbook;
use tokio::sync::Notify;
use tower::ServiceExt;
use xlsconvert::config::Config;
use xlsconvert::convert::{convert, ConvertError, Limits};

const TOKEN: &str = "token-de-teste-com-mais-de-32-caracteres";

fn from_ip(mut request: Request<Body>, last: u8) -> Request<Body> {
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [203, 0, 113, last],
            12345,
        ))));
    request
}

#[tokio::test]
async fn uploads_lentos_de_um_ip_nao_ocupam_todas_as_vagas() {
    let app = xlsconvert::router(config(&[
        ("XLSCONVERT_MODE", "apiconvert"),
        ("XLSCONVERT_MAX_INFLIGHT", "4"),
        ("XLSCONVERT_MAX_INFLIGHT_PER_IP", "2"),
    ]));
    let mut uploads = Vec::new();
    for _ in 0..2 {
        let started = Arc::new(Notify::new());
        let signal = started.clone();
        let body = Body::from_stream(stream::once(async move {
            signal.notify_one();
            std::future::pending::<Result<Bytes, std::io::Error>>().await
        }));
        uploads.push(tokio::spawn(
            app.clone().oneshot(from_ip(
                Request::post("/convert")
                    .header("content-type", "multipart/form-data; boundary=r")
                    .body(body)
                    .unwrap(),
                1,
            )),
        ));
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
    }
    let request = || {
        Request::post("/convert")
            .header("content-type", "multipart/form-data; boundary=r")
            .body(Body::from(multipart(include_bytes!("fixtures/tipos.xls"))))
            .unwrap()
    };
    let excess = app.clone().oneshot(from_ip(request(), 1)).await.unwrap();
    assert_eq!(excess.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let other_ip = app.clone().oneshot(from_ip(request(), 2)).await.unwrap();
    assert_eq!(other_ip.status(), StatusCode::OK);
    let cancelled = uploads.pop().unwrap();
    cancelled.abort();
    let _ = cancelled.await;
    let same_ip = app.oneshot(from_ip(request(), 1)).await.unwrap();
    assert_eq!(same_ip.status(), StatusCode::OK);
    for upload in uploads {
        upload.abort();
        let _ = upload.await;
    }
}

#[tokio::test]
async fn cota_por_ip_acompanha_os_bytes_da_resposta() {
    use http_body_util::BodyExt;
    let app = xlsconvert::router(config(&[
        ("XLSCONVERT_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_INFLIGHT", "4"),
        ("XLSCONVERT_MAX_INFLIGHT_PER_IP", "1"),
    ]));
    let request = || {
        Request::post("/convert")
            .header("x-internal-token", TOKEN)
            .body(Body::from(sheet((1, 0))))
            .unwrap()
    };
    let first = app.clone().oneshot(from_ip(request(), 1)).await.unwrap();
    let bytes = first.into_body().collect().await.unwrap().to_bytes();
    let excess = app.clone().oneshot(from_ip(request(), 1)).await.unwrap();
    assert_eq!(excess.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        app.clone()
            .oneshot(from_ip(request(), 2))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    drop(bytes);
    assert_eq!(
        app.oneshot(from_ip(request(), 1)).await.unwrap().status(),
        StatusCode::OK
    );
}

/// Mantém os registros BIFF/SST da fixture e reconstrói BOUNDSHEET/offsets.
/// Gráficos e módulos VB recebem substreams vazios; as planilhas recebem dados.
fn xls_with_sheets(types: &[u8]) -> (Vec<u8>, u64) {
    let mut compound =
        cfb::CompoundFile::open(Cursor::new(include_bytes!("fixtures/tipos.xls"))).unwrap();
    let mut input = Vec::new();
    compound
        .open_stream("/Workbook")
        .unwrap()
        .read_to_end(&mut input)
        .unwrap();
    let mut globals = Vec::new();
    let mut sheet_offset = None;
    let mut pos = 0;
    while pos + 4 <= input.len() {
        let opcode = u16::from_le_bytes(input[pos..pos + 2].try_into().unwrap());
        let len = u16::from_le_bytes(input[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let end = pos + 4 + len;
        if opcode == 0x0085 {
            if input[pos + 9] == 0 && sheet_offset.is_none() {
                sheet_offset =
                    Some(u32::from_le_bytes(input[pos + 4..pos + 8].try_into().unwrap()) as usize);
            }
        } else if opcode == 0x000A {
            break;
        } else {
            globals.extend_from_slice(&input[pos..end]);
        }
        pos = end;
    }
    let start = sheet_offset.unwrap();
    pos = start;
    let mut extent = 0;
    loop {
        let opcode = u16::from_le_bytes(input[pos..pos + 2].try_into().unwrap());
        let len = u16::from_le_bytes(input[pos + 2..pos + 4].try_into().unwrap()) as usize;
        if opcode == 0x0200 && len == 14 {
            let rows = u32::from_le_bytes(input[pos + 8..pos + 12].try_into().unwrap()) as u64;
            let cols = u16::from_le_bytes(input[pos + 14..pos + 16].try_into().unwrap()) as u64;
            extent = extent.max(rows * cols);
        }
        pos += 4 + len;
        if opcode == 0x000A {
            break;
        }
    }
    assert!(extent > 0);
    let sheet = &input[start..pos];
    let bof_len = 4 + u16::from_le_bytes(sheet[2..4].try_into().unwrap()) as usize;
    let streams: Vec<Vec<u8>> = types
        .iter()
        .map(|typ| {
            if *typ == 0 {
                sheet.to_vec()
            } else {
                let mut stream = sheet[..bof_len].to_vec();
                stream[6..8].copy_from_slice(
                    &(if *typ == 2 { 0x0020u16 } else { 0x0006u16 }).to_le_bytes(),
                );
                stream.extend_from_slice(&[0x0A, 0, 0, 0]);
                stream
            }
        })
        .collect();
    // BOUNDSHEET BIFF8: 8 bytes fixos + nome ASCII de 1 byte + header de 4.
    let mut offset = globals.len() + types.len() * 13 + 4;
    for (i, typ) in types.iter().enumerate() {
        globals.extend_from_slice(&[0x85, 0, 9, 0]);
        globals.extend_from_slice(&(offset as u32).to_le_bytes());
        globals.extend_from_slice(&[0, *typ, 1, 0, b'A' + i as u8]);
        offset += streams[i].len();
    }
    globals.extend_from_slice(&[0x0A, 0, 0, 0]);
    for stream in streams {
        globals.extend_from_slice(&stream);
    }
    let mut output =
        cfb::CompoundFile::create_with_version(cfb::Version::V3, Cursor::new(Vec::new())).unwrap();
    output
        .create_stream("/Workbook")
        .unwrap()
        .write_all(&globals)
        .unwrap();
    (output.into_inner().into_inner(), extent)
}

#[test]
fn xls_max_cells_nao_e_a_soma_das_abas() {
    use calamine::{Reader, Xls};
    let (input, extent) = xls_with_sheets(&[0, 0, 0]);
    let mut wb = Xls::new(Cursor::new(&input)).unwrap();
    assert_eq!(wb.sheet_names().len(), 3);
    assert!(!wb.worksheet_range_at(0).unwrap().unwrap().is_empty());
    let limits = Limits {
        max_cells: extent + 1,
        ..Limits::default()
    };
    assert!(extent * 3 > limits.max_cells);
    assert!(
        convert(&input, limits).is_ok(),
        "somente a primeira aba consome MAX_CELLS"
    );
}

#[test]
fn xls_grafico_e_vba_antes_da_planilha_nao_sao_convertidos() {
    use calamine::{Reader, Xlsx};
    let (input, extent) = xls_with_sheets(&[2, 6, 0]);
    let out = convert(
        &input,
        Limits {
            max_cells: extent + 1,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut wb = Xlsx::new(Cursor::new(out)).unwrap();
    assert!(!wb.worksheet_range_at(0).unwrap().unwrap().is_empty());
}

#[tokio::test]
async fn xls_tem_orcamento_independente_para_abrir_todas_as_abas() {
    use http_body_util::BodyExt;
    let (input, extent) = xls_with_sheets(&[0, 0, 0]);
    let parser_limit = (extent * 2).to_string();
    let mut cfg = config(&[
        ("XLSCONVERT_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_XLS_PARSE_CELLS", &parser_limit),
    ]);
    assert!(matches!(
        convert(&input, cfg.limits),
        Err(ConvertError::TooLarge("abas XLS"))
    ));
    cfg.worker_path = Some(env!("CARGO_BIN_EXE_xlsconvert").into());
    let request = || {
        Request::post("/convert")
            .header("x-internal-token", TOKEN)
            .body(Body::from(input.clone()))
            .unwrap()
    };
    let response = xlsconvert::router(cfg.clone())
        .oneshot(request())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("abas XLS"));
    cfg.limits.max_xls_parse_cells = extent * 3;
    assert_eq!(
        xlsconvert::router(cfg)
            .oneshot(request())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
}

#[test]
fn fronteiras_de_celulas_e_expansao_sao_independentes() {
    let input = sheet((1, 1));
    let mut zip = zip::ZipArchive::new(Cursor::new(&input)).unwrap();
    let expanded = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().size())
        .sum::<u64>();
    let limits = Limits {
        max_cells: 4,
        max_expanded_bytes: expanded,
        ..Limits::default()
    };
    assert!(convert(&input, limits).is_ok());
    assert!(matches!(
        convert(
            &input,
            Limits {
                max_cells: 3,
                ..limits
            }
        ),
        Err(ConvertError::TooLarge("células"))
    ));
    assert!(matches!(
        convert(
            &input,
            Limits {
                max_expanded_bytes: expanded - 1,
                ..limits
            }
        ),
        Err(ConvertError::TooLarge("expansão ZIP"))
    ));
}

#[tokio::test]
async fn resposta_pendente_mantem_a_vaga_de_memoria() {
    use http_body_util::BodyExt;
    let app = xlsconvert::router(config(&[
        ("XLSCONVERT_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_INFLIGHT", "1"),
    ]));
    let request = || {
        Request::post("/convert")
            .header("x-internal-token", TOKEN)
            .body(Body::from(sheet((1, 0))))
            .unwrap()
    };
    let first = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let excess = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(excess.status(), StatusCode::SERVICE_UNAVAILABLE);
    let held_bytes = first.into_body().collect().await.unwrap().to_bytes();
    let excess = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(excess.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(held_bytes);
    assert_eq!(
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn worker_preserva_o_erro_de_limite_da_api_interna() {
    use http_body_util::BodyExt;
    let mut cfg = config(&[("XLSCONVERT_TOKEN", TOKEN), ("XLSCONVERT_MAX_CELLS", "1")]);
    cfg.worker_path = Some(env!("CARGO_BIN_EXE_xlsconvert").into());
    let response = xlsconvert::router(cfg)
        .oneshot(
            Request::post("/convert")
                .header("x-internal-token", TOKEN)
                .body(Body::from(sheet((1, 0))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("planilha acima do limite (células)"));
}

#[test]
fn worker_sem_rede_converte_pela_entrada_padrao() {
    use std::process::{Command, Stdio};
    let mut child = Command::new(env!("CARGO_BIN_EXE_xlsconvert"))
        .args([
            "--conversion-worker",
            "5000000",
            "67108864",
            "20971520",
            "60",
            "512",
            "5000000",
        ])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&sheet((1, 0)))
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "worker: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout[0], 0);
    assert!(output.stdout[7..].starts_with(b"PK"));
}

#[cfg(unix)]
#[tokio::test]
async fn abort_do_worker_nao_derruba_o_servidor_nem_prende_a_vaga() {
    let mut cfg = config(&[
        ("XLSCONVERT_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_CONCURRENT", "1"),
    ]);
    cfg.worker_path = Some(env!("CARGO_BIN_EXE_xlsconvert").into());
    assert_eq!(cfg.worker_memory_bytes, 128 * 1024 * 1024);
    assert_eq!(cfg.limits.max_cells, 5_000_000);
    assert!(4_000_000 < cfg.limits.max_cells);
    let app = xlsconvert::router(cfg);
    let response = app
        .clone()
        .oneshot(
            Request::post("/convert")
                .header("x-internal-token", TOKEN)
                .body(Body::from(sheet((1999, 1999))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let health = app
        .clone()
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let response = app
        .oneshot(
            Request::post("/convert")
                .header("x-internal-token", TOKEN)
                .body(Body::from(sheet((1, 0))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn token_invalido_e_recusado_sem_ler_o_upload() {
    let app = xlsconvert::router(config(&[("XLSCONVERT_TOKEN", TOKEN)]));
    let body = Body::from_stream(stream::once(async {
        panic!("corpo não autorizado não deve ser lido");
        #[allow(unreachable_code)]
        Ok::<Bytes, std::io::Error>(Bytes::new())
    }));
    let response = app
        .oneshot(Request::post("/convert").body(body).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

fn config(pairs: &[(&str, &str)]) -> Config {
    Config::from_lookup(|k| {
        pairs
            .iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string())
    })
    .unwrap()
}

fn sheet(last: (u32, u16)) -> Vec<u8> {
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.write_string(0, 0, "H").unwrap();
    ws.write_string(last.0, last.1, "valor").unwrap();
    wb.save_to_buffer().unwrap()
}

fn rewrite_zip(input: &[u8], map: impl Fn(&str, Vec<u8>) -> Vec<u8>) -> Vec<u8> {
    let mut src = zip::ZipArchive::new(Cursor::new(input)).unwrap();
    let mut dst = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..src.len() {
        let mut entry = src.by_index(i).unwrap();
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        dst.start_file(
            &name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
        dst.write_all(&map(&name, bytes)).unwrap();
    }
    dst.finish().unwrap().into_inner()
}

#[test]
fn celulas_esparsas_nao_confiam_na_dimensao_declarada() {
    let input = rewrite_zip(&sheet((99, 99)), |name, bytes| {
        if name.ends_with("sheet1.xml") {
            String::from_utf8(bytes)
                .unwrap()
                .replace("A1:CV100", "A1:A2")
                .into_bytes()
        } else {
            bytes
        }
    });
    let limits = Limits {
        max_cells: 50,
        ..Limits::default()
    };
    assert!(matches!(
        convert(&input, limits),
        Err(ConvertError::TooLarge(_))
    ));
}

// O filho fica limitado a 512 MiB: uma regressão de alocação não mata a suíte.
#[cfg(unix)]
#[test]
fn grade_extrema_e_recusada_sem_abortar_o_processo() {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "grade_extrema_filho", "--nocapture"])
        .env("APICONVERT_RESOURCE_CHILD", "1");
    // SAFETY: setrlimit não aloca nem usa locks no processo após fork.
    unsafe {
        command.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 512 * 1024 * 1024,
                rlim_max: 512 * 1024 * 1024,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "filho abortou: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn grade_extrema_filho() {
    if std::env::var_os("APICONVERT_RESOURCE_CHILD").is_none() {
        return;
    }
    assert!(convert(&sheet((1, 0)), Limits::default()).is_ok());
    let input = sheet((1_048_575, 16_383));
    assert!(matches!(
        convert(&input, Limits::default()),
        Err(ConvertError::TooLarge(_))
    ));
    assert!(convert(&sheet((1, 0)), Limits::default()).is_ok());
    // DIMENSIONS causa reserve() na abertura do XLS, antes da primeira aba.
    let mut compound =
        cfb::CompoundFile::open(Cursor::new(include_bytes!("fixtures/tipos.xls"))).unwrap();
    let mut data = Vec::new();
    compound
        .open_stream("/Workbook")
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();
    let mut pos = 0;
    let mut patched = false;
    while pos + 4 <= data.len() {
        let opcode = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let len = u16::from_le_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if opcode == 0x0200 && len == 14 {
            data[pos + 4..pos + 8].copy_from_slice(&0u32.to_le_bytes());
            data[pos + 8..pos + 12].copy_from_slice(&u32::MAX.to_le_bytes());
            data[pos + 12..pos + 14].copy_from_slice(&0u16.to_le_bytes());
            data[pos + 14..pos + 16].copy_from_slice(&256u16.to_le_bytes());
            patched = true;
            break;
        }
        pos += 4 + len;
    }
    assert!(patched, "fixture tem DIMENSIONS");
    let mut output = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    output
        .create_stream("/Workbook")
        .unwrap()
        .write_all(&data)
        .unwrap();
    let xls = output.into_inner().into_inner();
    assert!(matches!(
        convert(&xls, Limits::default()),
        Err(ConvertError::TooLarge(_))
    ));
    assert!(convert(&sheet((1, 0)), Limits::default()).is_ok());
}

#[tokio::test]
async fn expansao_zip_e_limitada_antes_do_parser() {
    let input = rewrite_zip(&sheet((1, 0)), |name, bytes| {
        if name == "xl/sharedStrings.xml" {
            let huge = "x".repeat(2 * 1024 * 1024);
            format!("<sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><si><t>H</t></si><si><t>{huge}</t></si></sst>").into_bytes()
        } else {
            bytes
        }
    });
    let app = xlsconvert::router(config(&[
        ("XLSCONVERT_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_EXPANDED_MB", "1"),
    ]));
    let response = app
        .oneshot(
            Request::post("/convert")
                .header("x-internal-token", TOKEN)
                .body(Body::from(input))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

fn multipart(data: &[u8]) -> Vec<u8> {
    let mut body =
        b"--r\r\nContent-Disposition: form-data; name=\"arquivo\"; filename=\"teste.xls\"\r\n\r\n"
            .to_vec();
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n--r--\r\n");
    body
}

#[tokio::test]
async fn admissao_e_compartilhada_e_cancelamento_libera_vaga() {
    let app = xlsconvert::router(config(&[
        ("XLSCONVERT_MODE", "apiconvert"),
        ("XLSCONVERT_WEBP_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_INFLIGHT", "1"),
    ]));
    let read_started = Arc::new(Notify::new());
    let started = read_started.clone();
    let body = Body::from_stream(stream::once(async move {
        started.notify_one();
        std::future::pending::<Result<Bytes, std::io::Error>>().await
    }));
    let first = tokio::spawn(
        app.clone().oneshot(
            Request::post("/convert")
                .header("content-type", "multipart/form-data; boundary=r")
                .body(body)
                .unwrap(),
        ),
    );
    tokio::time::timeout(Duration::from_secs(2), read_started.notified())
        .await
        .unwrap();
    for (path, expected) in [
        ("/convert", StatusCode::INTERNAL_SERVER_ERROR),
        ("/ui/planilha", StatusCode::SERVICE_UNAVAILABLE),
        ("/webp", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "multipart/form-data; boundary=r")
                    .header("x-internal-token", TOKEN)
                    .body(Body::from(multipart(include_bytes!("fixtures/tipos.xls"))))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            expected,
            "{path} deve recusar antes de ler/converter"
        );
    }
    first.abort();
    let _ = first.await;
    let response = app
        .oneshot(
            Request::post("/convert")
                .header("content-type", "multipart/form-data; boundary=r")
                .body(Body::from(multipart(include_bytes!("fixtures/tipos.xls"))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn upload_lento_expira_e_libera_vaga() {
    let app = xlsconvert::router(config(&[
        ("XLSCONVERT_TOKEN", TOKEN),
        ("XLSCONVERT_MAX_INFLIGHT", "1"),
        ("XLSCONVERT_UPLOAD_TIMEOUT_SECS", "1"),
    ]));
    let body = Body::from_stream(stream::once(std::future::pending::<
        Result<Bytes, std::io::Error>,
    >()));
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        app.clone().oneshot(
            Request::post("/convert")
                .header("x-internal-token", TOKEN)
                .body(body)
                .unwrap(),
        ),
    )
    .await;
    assert!(result.is_ok(), "upload sem progresso deve expirar");
    assert_eq!(
        result.unwrap().unwrap().status(),
        StatusCode::GATEWAY_TIMEOUT
    );
    let response = app
        .oneshot(
            Request::post("/convert")
                .header("x-internal-token", TOKEN)
                .body(Body::from(sheet((1, 0))))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
