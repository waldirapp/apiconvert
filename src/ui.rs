//! Telas web (askama + htmx 4) para testar as conversões no navegador.
//!
//! | Rota | Uso |
//! |---|---|
//! | `GET /`, `/planilha`, `/imagem` | Páginas |
//! | `POST /ui/planilha` | Multipart `arquivo` → fragmento com números, prévia e link de download |
//! | `POST /ui/imagem` | Multipart `arquivo`, `max`, `q` → fragmento com o WebP, números e base64 |
//! | `GET /ui/baixar/{id}` | Download do resultado, por id aleatório de 128 bits |
//! | `GET /static/*` | htmx 4.0.0, CSS e JS embutidos no binário |
//!
//! As telas são abertas (sem login): a conversão não acessa banco, disco nem
//! outro serviço. O resultado fica em memória por `UI_TTL` e só é alcançável
//! pelo id devolvido a quem enviou; não há lista de conversões. As rotas
//! `POST` têm limite por IP e dividem o semáforo de conversões com as rotas
//! de planilha e imagem.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use askama::Template;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine;
use tokio::sync::Semaphore;

use crate::app::XLSX_MIME;
use crate::config::Config;
use crate::media_worker::{self, WorkerError};
use crate::webp::{self, WebpParams, MAX_SIDE_LIMIT};

/// Tempo que um resultado fica disponível para download.
pub const UI_TTL: Duration = Duration::from_secs(10 * 60);
const UI_MAX_BYTES: usize = 128 * 1024 * 1024;
const UI_MAX_ENTRIES: usize = 500;

const HTMX_JS: &[u8] = include_bytes!("../assets/htmx.min.js");
const APP_JS: &[u8] = include_bytes!("../assets/app.js");
const APP_CSS: &[u8] = include_bytes!("../assets/app.css");

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data: blob:; connect-src 'self'; form-action 'self'; \
base-uri 'none'; frame-ancestors 'none'";

struct Resultado {
    criado: Instant,
    mime: &'static str,
    nome: String,
    bytes: Bytes,
}

/// Resultados à espera do download, por id aleatório.
#[derive(Default)]
struct Resultados {
    mapa: Mutex<HashMap<String, Resultado>>,
}

impl Resultados {
    fn guardar(&self, mime: &'static str, nome: String, bytes: Bytes) -> Option<String> {
        if bytes.len() > UI_MAX_BYTES {
            return None;
        }
        let id = novo_id()?;
        let mut mapa = self.mapa.lock().unwrap_or_else(|p| p.into_inner());
        mapa.retain(|_, r| r.criado.elapsed() < UI_TTL);
        let mut total: usize = mapa.values().map(|r| r.bytes.len()).sum();
        while !mapa.is_empty()
            && (mapa.len() + 1 > UI_MAX_ENTRIES || total + bytes.len() > UI_MAX_BYTES)
        {
            let Some(antigo) = mapa
                .iter()
                .min_by_key(|(_, r)| r.criado)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(r) = mapa.remove(&antigo) {
                total -= r.bytes.len();
            }
        }
        mapa.insert(
            id.clone(),
            Resultado {
                criado: Instant::now(),
                mime,
                nome,
                bytes,
            },
        );
        Some(id)
    }

    fn buscar(&self, id: &str) -> Option<(&'static str, String, Bytes)> {
        let mut mapa = self.mapa.lock().unwrap_or_else(|p| p.into_inner());
        mapa.retain(|_, r| r.criado.elapsed() < UI_TTL);
        mapa.get(id)
            .map(|r| (r.mime, r.nome.clone(), r.bytes.clone()))
    }
}

/// 128 bits aleatórios em hexadecimal.
fn novo_id() -> Option<String> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).ok()?;
    Some(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Janela fixa de 1 minuto por IP.
struct Limite {
    por_minuto: u32,
    mapa: Mutex<HashMap<Option<IpAddr>, (Instant, u32)>>,
}

impl Limite {
    fn permite(&self, ip: Option<IpAddr>) -> bool {
        let mut mapa = self.mapa.lock().unwrap_or_else(|p| p.into_inner());
        let agora = Instant::now();
        mapa.retain(|_, (inicio, _)| agora.duration_since(*inicio) < Duration::from_secs(60));
        let entrada = mapa.entry(ip).or_insert((agora, 0));
        if entrada.1 >= self.por_minuto {
            return false;
        }
        entrada.1 += 1;
        true
    }
}

#[derive(Clone)]
struct UiState {
    cfg: Arc<Config>,
    permits: Arc<Semaphore>,
    resultados: Arc<Resultados>,
    limite: Arc<Limite>,
}

/// Rotas das telas; `permits` é o semáforo das conversões.
pub fn router(cfg: &Config, permits: Arc<Semaphore>) -> Router {
    let state = UiState {
        cfg: Arc::new(cfg.clone()),
        permits,
        resultados: Arc::new(Resultados::default()),
        limite: Arc::new(Limite {
            por_minuto: cfg.ui_rate_per_min,
            mapa: Mutex::new(HashMap::new()),
        }),
    };
    Router::new()
        .route("/", get(inicio))
        .route("/planilha", get(pagina_planilha))
        .route("/imagem", get(pagina_imagem))
        .route("/ui/planilha", post(converter_planilha))
        .route("/ui/imagem", post(converter_imagem))
        .route("/ui/baixar/{id}", get(baixar))
        .route("/static/{arquivo}", get(estatico))
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes))
        .layer(middleware::from_fn(cabecalhos_seguros))
        .with_state(state)
}

async fn cabecalhos_seguros(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    resp
}

fn ttl_min() -> u64 {
    UI_TTL.as_secs() / 60
}

#[derive(Template)]
#[template(path = "inicio.html")]
struct Inicio {
    titulo: &'static str,
    aba: &'static str,
    ttl_min: u64,
}

#[derive(Template)]
#[template(path = "planilha.html")]
struct PaginaPlanilha {
    titulo: &'static str,
    aba: &'static str,
    ttl_min: u64,
}

#[derive(Template)]
#[template(path = "imagem.html")]
struct PaginaImagem {
    titulo: &'static str,
    aba: &'static str,
    ttl_min: u64,
    max_lado: u32,
}

#[derive(Template)]
#[template(path = "_erro.html")]
struct Erro<'a> {
    mensagem: &'a str,
}

#[derive(Template)]
#[template(path = "_planilha_resultado.html")]
struct PlanilhaResultado {
    nome: String,
    id: String,
    linhas: usize,
    colunas: usize,
    bytes_entrada: String,
    bytes_saida: String,
    tempo_ms: u128,
    celulas: Vec<Vec<String>>,
    cortada: bool,
    colunas_previa: usize,
}

#[derive(Template)]
#[template(path = "_imagem_resultado.html")]
struct ImagemResultado {
    nome: String,
    nome_saida: String,
    id: String,
    base64: String,
    largura_orig: u32,
    altura_orig: u32,
    largura: u32,
    altura: u32,
    bytes_entrada: String,
    bytes_saida: String,
    reducao: String,
    transparente: bool,
    tempo_ms: u128,
}

fn html(t: impl Template, status: StatusCode) -> Response {
    match t.render() {
        Ok(corpo) => {
            let mut resp = (status, Html(corpo)).into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => {
            tracing::error!("falha ao montar a página: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "erro interno").into_response()
        }
    }
}

pub(crate) fn erro(status: StatusCode, mensagem: &str) -> Response {
    html(Erro { mensagem }, status)
}

/// Recusa antes ou durante a conversão, mostrada no fragmento de erro.
struct Falha(StatusCode, &'static str);

impl IntoResponse for Falha {
    fn into_response(self) -> Response {
        erro(self.0, self.1)
    }
}

async fn inicio() -> Response {
    html(
        Inicio {
            titulo: "Conversões",
            aba: "",
            ttl_min: ttl_min(),
        },
        StatusCode::OK,
    )
}

async fn pagina_planilha() -> Response {
    html(
        PaginaPlanilha {
            titulo: "Planilha",
            aba: "planilha",
            ttl_min: ttl_min(),
        },
        StatusCode::OK,
    )
}

async fn pagina_imagem() -> Response {
    html(
        PaginaImagem {
            titulo: "Imagem",
            aba: "imagem",
            ttl_min: ttl_min(),
            max_lado: MAX_SIDE_LIMIT,
        },
        StatusCode::OK,
    )
}

async fn estatico(Path(arquivo): Path<String>) -> Response {
    let (mime, corpo) = match arquivo.as_str() {
        "htmx.min.js" => ("text/javascript; charset=utf-8", HTMX_JS),
        "app.js" => ("text/javascript; charset=utf-8", APP_JS),
        "app.css" => ("text/css; charset=utf-8", APP_CSS),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        corpo,
    )
        .into_response()
}

async fn baixar(State(st): State<UiState>, Path(id): Path<String>) -> Response {
    let Some((mime, nome, bytes)) = st.resultados.buscar(&id) else {
        return erro(
            StatusCode::NOT_FOUND,
            "Resultado não encontrado ou expirado. Converta de novo.",
        );
    };
    let disposicao = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        nome_ascii(&nome),
        percent(&nome)
    );
    let mut resp = (StatusCode::OK, bytes).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&disposicao) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

/// Nome só com caracteres seguros para o `filename=` (o resto vira `_`).
fn nome_ascii(nome: &str) -> String {
    nome.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `filename*` (RFC 5987): UTF-8 com percent-encoding.
fn percent(nome: &str) -> String {
    nome.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn ip(req: &Request) -> Option<IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
}

/// Campos do formulário: o arquivo (nome e bytes) e os campos de texto.
struct Formulario {
    nome: String,
    arquivo: Bytes,
    campos: HashMap<String, String>,
}

async fn ler_formulario(req: Request) -> Result<Formulario, Falha> {
    use axum::extract::FromRequest;
    let mut mp = Multipart::from_request(req, &()).await.map_err(|_| {
        Falha(
            StatusCode::BAD_REQUEST,
            "Envio inválido: escolha um arquivo.",
        )
    })?;
    let mut form = Formulario {
        nome: String::new(),
        arquivo: Bytes::new(),
        campos: HashMap::new(),
    };
    loop {
        let campo = match mp.next_field().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => return Err(erro_corpo(e.status())),
        };
        let nome_campo = campo.name().unwrap_or_default().to_string();
        let nome_arquivo = campo.file_name().map(str::to_string);
        let dados = campo.bytes().await.map_err(|e| erro_corpo(e.status()))?;
        if nome_campo == "arquivo" {
            form.nome = nome_arquivo.unwrap_or_default();
            form.arquivo = dados;
        } else if dados.len() <= 64 {
            form.campos.insert(
                nome_campo,
                String::from_utf8_lossy(&dados).trim().to_string(),
            );
        }
    }
    if form.arquivo.is_empty() {
        return Err(Falha(
            StatusCode::BAD_REQUEST,
            "Escolha um arquivo para enviar.",
        ));
    }
    Ok(form)
}

fn erro_corpo(status: StatusCode) -> Falha {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        Falha(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Arquivo acima do tamanho máximo.",
        )
    } else {
        Falha(
            StatusCode::BAD_REQUEST,
            "Envio inválido: escolha um arquivo.",
        )
    }
}

/// Limite por IP, conferido antes de ler o corpo.
fn dentro_do_limite(st: &UiState, req: &Request) -> Result<(), Falha> {
    if st.limite.permite(ip(req)) {
        Ok(())
    } else {
        Err(Falha(
            StatusCode::TOO_MANY_REQUESTS,
            "Muitas conversões em pouco tempo. Aguarde um minuto.",
        ))
    }
}

/// Vaga no semáforo, pedida só depois de o upload chegar inteiro (um envio
/// lento não segura a vaga de uma conversão).
fn vaga(st: &UiState) -> Result<tokio::sync::OwnedSemaphorePermit, Falha> {
    st.permits.clone().try_acquire_owned().map_err(|_| {
        Falha(
            StatusCode::SERVICE_UNAVAILABLE,
            "Servidor ocupado com outras conversões. Tente de novo em instantes.",
        )
    })
}

fn tamanho(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn worker_failure(e: WorkerError) -> Falha {
    match e {
        WorkerError::Timeout => Falha(
            StatusCode::GATEWAY_TIMEOUT,
            "A conversão excedeu o tempo limite.",
        ),
        WorkerError::TooLarge => Falha(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Resultado grande demais para processar.",
        ),
        _ => Falha(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Falha interna na conversão.",
        ),
    }
}

async fn converter_planilha(State(st): State<UiState>, req: Request) -> Response {
    if let Err(r) = dentro_do_limite(&st, &req) {
        return r.into_response();
    }
    let form = match ler_formulario(req).await {
        Ok(f) => f,
        Err(r) => return r.into_response(),
    };
    let permit = match vaga(&st) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let nome = crate::compat::output_name(&form.nome).unwrap_or_else(|| "planilha.xlsx".into());
    let entrada = form.arquivo.len();
    let limits = st.cfg.limits.with_timeout(st.cfg.timeout);
    let inicio = Instant::now();
    let arquivo = form.arquivo;
    // A prévia lê o .xlsx gerado e mantém a vaga de CPU e o prazo.
    let permit = Arc::new(permit);
    let (saida, (linhas, colunas, celulas, cortada, colunas_previa)) =
        match tokio::time::timeout(st.cfg.timeout, async {
            let converted =
                crate::conversion_worker::run(arquivo, limits, &st.cfg, permit.clone()).await;
            match converted {
                Ok(x) => {
                    let x = Bytes::from(x);
                    let p = media_worker::preview(x.clone(), &st.cfg, permit)
                        .await
                        .map_err(worker_failure)?;
                    Ok::<_, Falha>(Ok((x, p)))
                }
                Err(e) => Ok(Err(e)),
            }
        })
        .await
        {
            Ok(Ok(Ok(x))) => x,
            Ok(Ok(Err(e))) => {
                tracing::info!(bytes = entrada, "tela: planilha recusada: {e}");
                return erro(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &primeira_maiuscula(&e.to_string()),
                );
            }
            Ok(Err(r)) => return r.into_response(),
            Err(_) => {
                return erro(
                    StatusCode::GATEWAY_TIMEOUT,
                    "A conversão excedeu o tempo limite.",
                )
            }
        };
    let tempo_ms = inicio.elapsed().as_millis();
    tracing::info!(
        bytes = entrada,
        saida = saida.len(),
        elapsed_ms = tempo_ms,
        "tela: planilha convertida"
    );

    let bytes_saida = tamanho(saida.len());
    let Some(id) = st.resultados.guardar(XLSX_MIME, nome.clone(), saida) else {
        return erro(
            StatusCode::INSUFFICIENT_STORAGE,
            "Resultado grande demais para guardar.",
        );
    };
    html(
        PlanilhaResultado {
            nome,
            id,
            linhas,
            colunas,
            bytes_entrada: tamanho(entrada),
            bytes_saida,
            tempo_ms,
            celulas,
            cortada,
            colunas_previa,
        },
        StatusCode::OK,
    )
}

fn primeira_maiuscula(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(p) => p.to_uppercase().chain(c).collect::<String>() + ".",
        None => String::new(),
    }
}

async fn converter_imagem(State(st): State<UiState>, req: Request) -> Response {
    if let Err(r) = dentro_do_limite(&st, &req) {
        return r.into_response();
    }
    let form = match ler_formulario(req).await {
        Ok(f) => f,
        Err(r) => return r.into_response(),
    };
    let permit = match vaga(&st) {
        Ok(p) => p,
        Err(r) => return r.into_response(),
    };
    let mut params = WebpParams::default();
    if let Some(v) = form.campos.get("max").filter(|v| !v.is_empty()) {
        match v.parse() {
            Ok(n) => params.max_side = n,
            Err(_) => return erro(StatusCode::BAD_REQUEST, "Lado maior inválido."),
        }
    }
    if let Some(v) = form.campos.get("q").filter(|v| !v.is_empty()) {
        match v.parse::<f32>() {
            Ok(q) if q.is_finite() => params.quality = q,
            _ => return erro(StatusCode::BAD_REQUEST, "Qualidade inválida."),
        }
    }
    let base = form
        .nome
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string();
    let nome = if base.is_empty() {
        "imagem".to_string()
    } else {
        base
    };
    let stem = nome.rfind('.').map_or(nome.as_str(), |i| &nome[..i]);
    let nome_saida = format!("{stem}.webp");
    let entrada = form.arquivo.len();
    let inicio = Instant::now();
    let arquivo = form.arquivo;
    let r = media_worker::image(arquivo, params, &st.cfg, Arc::new(permit)).await;
    let ((orig, dims, transparente, base64, out), tempo_ms) = match r {
        Ok(output) => {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&output.bytes);
            (
                (
                    output.info.original,
                    output.info.dimensions,
                    output.info.transparent,
                    b64,
                    output.bytes,
                ),
                inicio.elapsed().as_millis(),
            )
        }
        Err(WorkerError::Image(e)) => {
            tracing::info!(bytes = entrada, "tela: imagem recusada: {e}");
            let status = match e {
                webp::WebpError::Param(_) | webp::WebpError::Empty => StatusCode::BAD_REQUEST,
                webp::WebpError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                webp::WebpError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            return erro(status, &primeira_maiuscula(&e.to_string()));
        }
        Err(e) => return worker_failure(e).into_response(),
    };
    tracing::info!(
        bytes = entrada,
        saida = out.len(),
        elapsed_ms = tempo_ms,
        "tela: imagem convertida"
    );

    let (largura, altura) = dims.unwrap_or((0, 0));
    let (largura_orig, altura_orig) = orig.unwrap_or((0, 0));
    let reducao = if entrada > 0 {
        format!("{:.0}%", 100.0 - out.len() as f64 * 100.0 / entrada as f64)
    } else {
        "—".into()
    };
    let bytes_saida = tamanho(out.len());
    let Some(id) = st
        .resultados
        .guardar("image/webp", nome_saida.clone(), Bytes::from(out))
    else {
        return erro(
            StatusCode::INSUFFICIENT_STORAGE,
            "Resultado grande demais para guardar.",
        );
    };
    html(
        ImagemResultado {
            nome,
            nome_saida,
            id,
            base64,
            largura_orig,
            altura_orig,
            largura,
            altura,
            bytes_entrada: tamanho(entrada),
            bytes_saida,
            reducao,
            transparente,
            tempo_ms,
        },
        StatusCode::OK,
    )
}
