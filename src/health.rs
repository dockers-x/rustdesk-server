use hbb_common::{
    anyhow::Context,
    log,
    tokio::{
        self,
        net::{TcpListener, TcpStream},
        sync::{mpsc, oneshot},
        task::{JoinHandle, JoinSet},
        time::timeout,
    },
    ResultType,
};
use hyper::{server::conn::Http, service::service_fn, Body, Method, Request, Response, StatusCode};
use std::{convert::Infallible, net::Ipv4Addr, time::Duration};

const MAX_CONNECTIONS: usize = 16;
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);
const READY_TIMEOUT: Duration = Duration::from_millis(500);

/// Loopback health server and the readiness requests delivered to the service loop.
pub struct Health {
    requests: mpsc::Receiver<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Health {
    /// Starts serving health probes when the configured port is nonzero.
    pub async fn bind(setting: &str, value: &str) -> ResultType<Self> {
        let port = match value.trim() {
            "" => 0,
            value => value
                .parse::<u16>()
                .with_context(|| format!("Invalid {setting} port"))?,
        };
        let (sender, requests) = mpsc::channel(MAX_CONNECTIONS);
        let task = if port == 0 {
            None
        } else {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
                .await
                .with_context(|| format!("Bind {setting} on 127.0.0.1:{port}"))?;
            log::info!(target: "health.listening", "Health probes listening on 127.0.0.1:{port}");
            Some(tokio::spawn(serve(listener, sender)))
        };
        Ok(Self { requests, task })
    }

    /// Waits for a readiness request. Poll only from the initialized service loop.
    pub async fn next_probe(&mut self) -> oneshot::Sender<()> {
        match self.requests.recv().await {
            Some(probe) => probe,
            None => std::future::pending().await,
        }
    }
}

impl Drop for Health {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn serve(listener: TcpListener, probes: mpsc::Sender<oneshot::Sender<()>>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok((stream, _)) => {
                        connections.spawn(serve_connection(stream, probes.clone()));
                    }
                    Err(err) => {
                        log::warn!(target: "health.accept_failed", "Health listener accept failed: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
}

async fn serve_connection(stream: TcpStream, probes: mpsc::Sender<oneshot::Sender<()>>) {
    let service = service_fn(move |request| answer(request, probes.clone()));
    let connection = Http::new()
        .http1_only(true)
        .http1_keep_alive(false)
        .serve_connection(stream, service);
    let _ = timeout(HTTP_TIMEOUT, connection).await;
}

async fn answer(
    request: Request<Body>,
    probes: mpsc::Sender<oneshot::Sender<()>>,
) -> Result<Response<Body>, Infallible> {
    let (status, body) = match (request.method(), request.uri().path()) {
        (&Method::GET, "/livez") => (StatusCode::OK, "ok\n"),
        (&Method::GET, "/readyz") => {
            let (reply, response) = oneshot::channel();
            if probes.try_send(reply).is_ok()
                && matches!(timeout(READY_TIMEOUT, response).await, Ok(Ok(())))
            {
                (StatusCode::OK, "ok\n")
            } else {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "service loop unavailable\n",
                )
            }
        }
        _ => (StatusCode::NOT_FOUND, "not found\n"),
    };
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    Ok(response)
}
