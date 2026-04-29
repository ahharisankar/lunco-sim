//! Transport adapters.
#[cfg(feature = "transport-http")]
mod http;
#[cfg(feature = "transport-http")]
pub use http::*;

#[cfg(feature = "transport-http")]
#[derive(Debug, Clone)]
pub struct HttpServerConfig {
    pub port: u16,
}

#[cfg(feature = "transport-http")]
pub struct BridgeMessage {
    pub request: crate::schema::ApiRequest,
    pub reply: tokio::sync::oneshot::Sender<crate::schema::ApiResponse>,
}

/// Wakes the host event loop after pushing a message into the
/// bridge's mpsc. Without this, an HTTP request handed to the bridge
/// only gets drained on the next Bevy tick — which, in reactive
/// `WinitSettings`, may not arrive for a full second. The waker is
/// optional so headless tests / non-winit hosts can still use the
/// bridge without paying for a winit dep.
#[cfg(feature = "transport-http")]
pub type ApiWaker = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(feature = "transport-http")]
#[derive(Clone)]
pub struct HttpBridge {
    pub tx: tokio::sync::mpsc::UnboundedSender<BridgeMessage>,
    pub waker: Option<ApiWaker>,
}

#[cfg(feature = "transport-http")]
impl HttpBridge {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<BridgeMessage>) -> Self {
        Self { tx, waker: None }
    }

    pub fn with_waker(mut self, waker: ApiWaker) -> Self {
        self.waker = Some(waker);
        self
    }

    pub async fn execute(&self, request: crate::schema::ApiRequest) -> Result<crate::schema::ApiResponse, ()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx.send(BridgeMessage { request, reply: tx }).map_err(|_| ())?;
        if let Some(waker) = &self.waker {
            waker();
        }
        rx.await.map_err(|_| ())
    }
}

#[cfg(feature = "transport-http")]
pub fn spawn_server(config: HttpServerConfig, bridge: HttpBridge) {
    let port = config.port;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let app = axum::Router::new()
                .route("/api/commands", axum::routing::post(http::handle_api_commands))
                .with_state(bridge);
            
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{}", port)).await.unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
}
