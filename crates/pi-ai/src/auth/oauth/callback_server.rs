//! Loopback OAuth redirect handler shared by the browser sign-in flows
//! (port of `auth/oauth/callback-server.ts`).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use crate::utils::wasm::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::auth::types::{AuthPrompt, AuthPromptKind, ProviderAuthInteraction};
use crate::utils::oauth_page::{oauth_error_html, oauth_success_html};

/// Finishes the sign-in with the received code before the browser page is sent.
pub type CompleteFn<T> = Arc<dyn Fn(String) -> BoxFuture<'static, anyhow::Result<T>> + Send + Sync>;

pub struct OAuthCallbackServerOptions<T> {
    /// Provider name used on the browser page, for example `OpenAI`.
    pub provider_name: String,
    /// Address to listen on.
    pub host: String,
    /// Port to listen on; `0` picks a free port.
    pub port: u16,
    pub path: String,
    /// Host in `redirect_uri` when it differs from `host`, for example `localhost`.
    pub redirect_host: Option<String>,
    /// Expected `state` parameter. `None` when the provider does not send one.
    pub state: Option<String>,
    pub complete: CompleteFn<T>,
    pub signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
}

struct State<T> {
    claimed: bool,
    settled: bool,
    result: Option<Result<Option<T>, String>>,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    notify: Notify,
    shutdown: CancellationToken,
    listener: SharedListener,
}

impl<T: Clone> Shared<T> {
    fn finish(&self, result: Result<Option<T>, String>) {
        let mut state = self.state.lock();
        if state.settled {
            return;
        }
        state.settled = true;
        state.result = Some(result);
        drop(state);
        self.notify.notify_waiters();
    }
}

/// A running callback server.
pub struct OAuthCallbackServer<T> {
    pub redirect_uri: String,
    shared: Arc<Shared<T>>,
}

impl<T: Clone + Send + Sync + 'static> OAuthCallbackServer<T> {
    /// Resolves with the result of `complete`, or `None` after [`cancel`](Self::cancel). Fails when
    /// the provider redirects with an error, `complete` fails, the signal aborts, or the timeout elapses.
    pub async fn wait(&self) -> anyhow::Result<Option<T>> {
        loop {
            let notified = self.shared.notify.notified();
            if let Some(result) = self.shared.state.lock().result.clone() {
                return result.map_err(|message| anyhow::anyhow!(message));
            }
            notified.await;
        }
    }

    /// Stop waiting for the browser unless a callback is already being completed.
    pub fn cancel(&self) {
        let claimed = self.shared.state.lock().claimed;
        if !claimed {
            self.shared.finish(Ok(None));
        }
    }

    pub fn close(&self) {
        self.shared
            .finish(Err("OAuth callback server closed".into()));
        self.shared.listener.close();
        self.shared.shutdown.cancel();
    }
}

impl<T> Drop for OAuthCallbackServer<T> {
    fn drop(&mut self) {
        self.shared.listener.close();
        self.shared.shutdown.cancel();
    }
}

/// A listener that can be closed synchronously (like `server.close()`), releasing the port at once.
#[derive(Clone)]
pub(crate) struct SharedListener(Arc<Mutex<Option<TcpListener>>>);

impl SharedListener {
    pub(crate) fn new(listener: TcpListener) -> Self {
        Self(Arc::new(Mutex::new(Some(listener))))
    }

    /// The next connection; `None` once closed.
    pub(crate) async fn accept(&self) -> Option<std::io::Result<TcpStream>> {
        futures::future::poll_fn(|cx| {
            let guard = self.0.lock();
            match guard.as_ref() {
                Some(listener) => listener
                    .poll_accept(cx)
                    .map(|r| Some(r.map(|(socket, _)| socket))),
                None => std::task::Poll::Ready(None),
            }
        })
        .await
    }

    pub(crate) fn close(&self) {
        self.0.lock().take();
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "",
    }
}

/// Write an HTML page response and close the connection.
pub(crate) async fn send_page(socket: &mut TcpStream, status: u16, html: &str, no_store: bool) {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n",
        reason_phrase(status),
        html.len()
    );
    if no_store {
        head.push_str("cache-control: no-store\r\n");
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(html.as_bytes()).await;
    let _ = socket.flush().await;
    let _ = socket.shutdown().await;
}

/// Read an HTTP request head; returns the method and target.
pub(crate) async fn read_request_line(socket: &mut TcpStream) -> Option<(String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let mut parts = head.lines().next()?.split_whitespace();
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

async fn handle_connection<T: Clone + Send + Sync + 'static>(
    mut socket: TcpStream,
    shared: Arc<Shared<T>>,
    path: String,
    state: Option<String>,
    provider_name: String,
    complete: CompleteFn<T>,
) {
    let Some((method, target)) = read_request_line(&mut socket).await else {
        return;
    };
    let url = url::Url::parse("http://localhost").and_then(|base| base.join(&target));
    let Ok(url) = url else {
        send_page(
            &mut socket,
            404,
            &oauth_error_html("Callback route not found.", None),
            true,
        )
        .await;
        return;
    };
    if method != "GET" || url.path() != path {
        send_page(
            &mut socket,
            404,
            &oauth_error_html("Callback route not found.", None),
            true,
        )
        .await;
        return;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    if let Some(expected) = &state
        && param("state").as_deref() != Some(expected.as_str())
    {
        send_page(
            &mut socket,
            400,
            &oauth_error_html("State mismatch.", None),
            true,
        )
        .await;
        return;
    }
    enum Decision {
        AlreadyHandled,
        ProviderError(String),
        MissingCode,
        Complete(String),
    }
    let decision = {
        let mut guard = shared.state.lock();
        if guard.claimed || guard.settled {
            Decision::AlreadyHandled
        } else if let Some(error) = param("error") {
            Decision::ProviderError(param("error_description").unwrap_or(error))
        } else if let Some(code) = param("code") {
            guard.claimed = true;
            Decision::Complete(code)
        } else {
            Decision::MissingCode
        }
    };
    match decision {
        Decision::AlreadyHandled => {
            send_page(
                &mut socket,
                409,
                &oauth_error_html("This sign-in has already been handled.", None),
                true,
            )
            .await;
        }
        Decision::ProviderError(description) => {
            send_page(
                &mut socket,
                400,
                &oauth_error_html(
                    &format!("{provider_name} authorization failed."),
                    Some(&description),
                ),
                true,
            )
            .await;
            shared.finish(Err(format!(
                "{provider_name} authorization failed: {description}"
            )));
        }
        Decision::MissingCode => {
            send_page(
                &mut socket,
                400,
                &oauth_error_html("Missing authorization code.", None),
                true,
            )
            .await;
        }
        Decision::Complete(code) => match complete(code).await {
            Ok(value) => {
                let message = format!("Signed in to {provider_name}. You may now close this page.");
                send_page(&mut socket, 200, &oauth_success_html(&message), true).await;
                shared.finish(Ok(Some(value)));
            }
            Err(error) => {
                let message = error.to_string();
                send_page(
                    &mut socket,
                    502,
                    &oauth_error_html(&format!("{provider_name} sign-in failed."), Some(&message)),
                    true,
                )
                .await;
                shared.finish(Err(message));
            }
        },
    }
}

/// Start a loopback callback server.
pub async fn start_oauth_callback_server<T: Clone + Send + Sync + 'static>(
    options: OAuthCallbackServerOptions<T>,
) -> anyhow::Result<OAuthCallbackServer<T>> {
    if options.signal.as_ref().is_some_and(|s| s.is_cancelled()) {
        anyhow::bail!("Login cancelled");
    }
    let host = options
        .host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let listener = TcpListener::bind((host.as_str(), options.port)).await?;
    let port = listener.local_addr()?.port();
    let listener = SharedListener::new(listener);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            claimed: false,
            settled: false,
            result: None,
        }),
        notify: Notify::new(),
        shutdown: CancellationToken::new(),
        listener: listener.clone(),
    });

    let accept_shared = shared.clone();
    let path = options.path.clone();
    let state = options.state.clone();
    let provider_name = options.provider_name.clone();
    let complete = options.complete.clone();
    crate::utils::wasm::spawn(async move {
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = accept_shared.shutdown.cancelled() => return,
            };
            let Some(accepted) = accepted else { return };
            match accepted {
                Ok(socket) => {
                    crate::utils::wasm::spawn(handle_connection(
                        socket,
                        accept_shared.clone(),
                        path.clone(),
                        state.clone(),
                        provider_name.clone(),
                        complete.clone(),
                    ));
                }
                Err(error) => {
                    accept_shared.finish(Err(error.to_string()));
                    return;
                }
            }
        }
    });

    if let Some(signal) = options.signal.clone() {
        let abort_shared = shared.clone();
        crate::utils::wasm::spawn(async move {
            tokio::select! {
                _ = signal.cancelled() => abort_shared.finish(Err("Login cancelled".into())),
                _ = abort_shared.shutdown.cancelled() => {}
            }
        });
    }
    if let Some(timeout_ms) = options.timeout_ms {
        let timer_shared = shared.clone();
        let provider_name = options.provider_name.clone();
        crate::utils::wasm::spawn(async move {
            tokio::select! {
                _ = crate::utils::wasm::sleep(Duration::from_millis(timeout_ms)) => {
                    timer_shared.finish(Err(format!("{provider_name} sign-in timed out")));
                }
                _ = timer_shared.shutdown.cancelled() => {}
            }
        });
    }

    let redirect_host = options
        .redirect_host
        .clone()
        .unwrap_or_else(|| options.host.clone());
    let redirect_host = if redirect_host.contains(':') {
        format!("[{redirect_host}]")
    } else {
        redirect_host
    };
    Ok(OAuthCallbackServer {
        redirect_uri: format!("http://{redirect_host}:{port}{}", options.path),
        shared,
    })
}

/// The outcome of [`wait_for_callback_or_manual_input`].
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackOrManual<T> {
    Callback(T),
    Manual(String),
}

/// Wait for the browser callback, or for the user to paste the code or redirect URL when the
/// browser cannot reach the loopback server. Without a callback server only the prompt is used.
pub async fn wait_for_callback_or_manual_input<T: Clone + Send + Sync + 'static>(
    interaction: &ProviderAuthInteraction,
    callback: Option<&OAuthCallbackServer<T>>,
    message: &str,
    placeholder: &str,
) -> anyhow::Result<CallbackOrManual<T>> {
    let manual_abort = CancellationToken::new();
    let prompt = AuthPrompt {
        kind: AuthPromptKind::ManualCode,
        message: message.to_string(),
        placeholder: Some(placeholder.to_string()),
        options: Vec::new(),
        signal: Some(manual_abort.clone()),
    };
    let result = async {
        let mut manual = std::pin::pin!(interaction.interaction.prompt(prompt));
        let Some(callback) = callback else {
            return Ok(CallbackOrManual::Manual(manual.await?));
        };
        let mut wait = std::pin::pin!(callback.wait());
        let mut manual_result: Option<anyhow::Result<String>> = None;
        let value = loop {
            tokio::select! {
                result = &mut manual, if manual_result.is_none() => {
                    manual_result = Some(result);
                    callback.cancel();
                }
                value = &mut wait => break value?,
            }
        };
        if let Some(Err(error)) = manual_result {
            return Err(error);
        }
        if let Some(value) = value {
            return Ok(CallbackOrManual::Callback(value));
        }
        let input = match manual_result {
            Some(result) => result?,
            None => manual.await?,
        };
        Ok(CallbackOrManual::Manual(input))
    }
    .await;
    manual_abort.cancel();
    result
}
