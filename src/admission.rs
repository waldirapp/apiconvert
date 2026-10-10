//! Admissão compartilhada antes da leitura do corpo. Cada vaga cobre upload,
//! espera e resposta, sem ocupar antecipadamente uma vaga de CPU.
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use http_body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;

use crate::config::{Config, Mode};

#[derive(Clone)]
struct Admission {
    cfg: Arc<Config>,
    permits: Arc<Semaphore>,
}

pub(crate) fn apply(router: Router, cfg: &Config) -> Router {
    router.layer(middleware::from_fn_with_state(
        Admission {
            cfg: Arc::new(cfg.clone()),
            permits: Arc::new(Semaphore::new(cfg.max_inflight)),
        },
        admit,
    ))
}

fn failure(cfg: &Config, path: &str, status: StatusCode, code: &str, message: &str) -> Response {
    if cfg.mode == Mode::Apiconvert && path == "/convert" {
        crate::compat::erro_gravar()
    } else if path.starts_with("/ui/") {
        crate::ui::erro(status, message)
    } else {
        crate::app::error(status, code, message)
    }
}

async fn admit(State(st): State<Admission>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let conversion_route = match path.as_str() {
        "/convert" => true,
        "/webp" => st.cfg.webp_token.is_some(),
        "/ui/planilha" | "/ui/imagem" => st.cfg.ui,
        _ => false,
    };
    if req.method() != axum::http::Method::POST || !conversion_route {
        return next.run(req).await;
    }
    let token = match path.as_str() {
        "/convert" if st.cfg.mode == Mode::Interno => Some(st.cfg.token.as_str()),
        "/webp" => st.cfg.webp_token.as_deref(),
        _ => None,
    };
    if let Some(token) = token {
        if !crate::app::token_ok(req.headers(), token) {
            return crate::app::error(
                StatusCode::UNAUTHORIZED,
                "nao_autorizado",
                "token interno ausente ou inválido",
            );
        }
    }
    let Ok(permit) = st.permits.clone().try_acquire_owned() else {
        return failure(
            &st.cfg,
            &path,
            StatusCode::SERVICE_UNAVAILABLE,
            "ocupado",
            "Servidor ocupado com outros uploads. Tente de novo em instantes.",
        );
    };
    let expired = Arc::new(AtomicBool::new(false));
    let body = std::mem::replace(req.body_mut(), Body::empty());
    *req.body_mut() = Body::new(UploadBody {
        body,
        deadline: Box::pin(tokio::time::sleep(st.cfg.upload_timeout)),
        expired: expired.clone(),
        done: false,
    });
    let mut response = next.run(req).await;
    if expired.load(Ordering::Relaxed) {
        response = failure(
            &st.cfg,
            &path,
            StatusCode::GATEWAY_TIMEOUT,
            "tempo_esgotado",
            "O envio do arquivo excedeu o tempo limite.",
        );
    }
    let body = std::mem::replace(response.body_mut(), Body::empty());
    *response.body_mut() = Body::new(InFlightBody {
        body,
        permit: Arc::new(permit),
    });
    response
}

/// A vaga acompanha também os bytes entregues ao transporte. Liberá-la ao
/// criar a resposta deixaria clientes lentos acumular resultados na memória.
struct InFlightBody {
    body: Body,
    permit: Arc<OwnedSemaphorePermit>,
}

struct InFlightBytes {
    bytes: Bytes,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl AsRef<[u8]> for InFlightBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl HttpBody for InFlightBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                let permit = self.permit.clone();
                Poll::Ready(Some(Ok(frame.map_data(|bytes| {
                    Bytes::from_owner(InFlightBytes {
                        bytes,
                        _permit: permit,
                    })
                }))))
            }
            other => other,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// O prazo limita a leitura, não a conversão. A vaga é liberada por RAII
/// também em erro/cancelamento; a vaga de CPU continua com o worker.
struct UploadBody {
    body: Body,
    deadline: Pin<Box<Sleep>>,
    expired: Arc<AtomicBool>,
    done: bool,
}

impl HttpBody for UploadBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            self.expired.store(true, Ordering::Relaxed);
            self.done = true;
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "upload timeout",
            ))));
        }
        match Pin::new(&mut self.body).poll_frame(cx) {
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                self.done = true;
                Poll::Ready(Some(Err(std::io::Error::other(e))))
            }
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done || self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
