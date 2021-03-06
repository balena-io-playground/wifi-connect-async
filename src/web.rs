use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use axum::{
    extract,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;

use serde::Serialize;

use crate::network::{Command, CommandRequest, CommandResponce};
use crate::nl80211;

pub enum AppResponse {
    Network(CommandResponce),
    Error(anyhow::Error),
}

#[derive(Serialize)]
pub struct AppErrors {
    pub errors: Vec<String>,
}

impl AppErrors {
    fn new(errors: Vec<String>) -> Self {
        Self { errors }
    }
}

struct MainState {
    glib_sender: glib::Sender<CommandRequest>,
    shutdown_opt: Mutex<Option<oneshot::Sender<()>>>,
}

pub async fn run_web_loop(glib_sender: glib::Sender<CommandRequest>) {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let shared_state = Arc::new(MainState {
        glib_sender: glib_sender.clone(),
        shutdown_opt: Mutex::new(Some(shutdown_tx)),
    });

    let app = Router::new()
        .route("/", get(usage))
        .route("/check-connectivity", get(check_connectivity))
        .route("/list-connections", get(list_connections))
        .route("/list-wifi-networks", get(list_wifi_networks))
        .route("/shutdown", get(shutdown))
        .route("/stop", get(stop))
        .route("/scan", get(scan))
        .layer(Extension(shared_state));

    let server =
        axum::Server::bind(&"0.0.0.0:3000".parse().unwrap()).serve(app.into_make_service());

    let graceful = server.with_graceful_shutdown(shutdown_signal(shutdown_rx, glib_sender));

    println!("Web server starting...");

    graceful.await.unwrap();
}

async fn shutdown_signal(
    shutdown_rx: oneshot::Receiver<()>,
    glib_sender: glib::Sender<CommandRequest>,
) {
    let mut interrupt = signal(SignalKind::interrupt()).unwrap();
    let mut terminate = signal(SignalKind::terminate()).unwrap();
    let mut quit = signal(SignalKind::quit()).unwrap();
    let mut hangup = signal(SignalKind::hangup()).unwrap();

    tokio::select! {
        _ = shutdown_rx => {},
        _ = interrupt.recv() => println!("SIGINT received"),
        _ = terminate.recv() => println!("SIGTERM received"),
        _ = quit.recv() => println!("SIGQUIT received"),
        _ = hangup.recv() => println!("SIGHUP received"),
    }

    println!("Shutting down...");

    send_command(&glib_sender, Command::Stop).await;

    println!("Quit.");
}

async fn usage() -> &'static str {
    "Use /check-connectivity or /list-connections\n"
}

async fn check_connectivity(state: extract::Extension<Arc<MainState>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, Command::CheckConnectivity)
        .await
        .into_response()
}

async fn list_connections(state: extract::Extension<Arc<MainState>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, Command::ListConnections)
        .await
        .into_response()
}

async fn list_wifi_networks(state: extract::Extension<Arc<MainState>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, Command::ListWiFiNetworks)
        .await
        .into_response()
}

async fn shutdown(mut state: extract::Extension<Arc<MainState>>) -> impl IntoResponse {
    let response = send_command(&state.0.glib_sender, Command::Shutdown)
        .await
        .into_response();

    issue_shutdwon(&mut state.0).await;

    response
}

async fn stop(state: extract::Extension<Arc<MainState>>) -> impl IntoResponse {
    send_command(&state.0.glib_sender, Command::Stop)
        .await
        .into_response()
}

async fn scan(_: extract::Extension<Arc<MainState>>) -> impl IntoResponse {
    let stations = nl80211::scan::scan("wlan0").await.unwrap();
    (StatusCode::OK, Json(stations)).into_response()
}

async fn issue_shutdwon(state: &mut Arc<MainState>) {
    if let Some(shutdown_tx) = state.shutdown_opt.lock().unwrap().take() {
        shutdown_tx.send(()).ok();
    }
}

async fn send_command(glib_sender: &glib::Sender<CommandRequest>, command: Command) -> AppResponse {
    let (responder, receiver) = oneshot::channel();

    let action = match command {
        Command::CheckConnectivity => "check connectivity",
        Command::ListConnections => "list actions",
        Command::ListWiFiNetworks => "list WiFi networks",
        Command::Shutdown => "shutdown",
        Command::Stop => "stop",
    };

    glib_sender
        .send(CommandRequest::new(responder, command))
        .unwrap();

    receive_network_thread_response(receiver, action)
        .await
        .into()
}

async fn receive_network_thread_response(
    receiver: oneshot::Receiver<Result<CommandResponce>>,
    action: &str,
) -> Result<CommandResponce> {
    let result = receiver
        .await
        .context("Failed to receive network thread response");

    result
        .and_then(|r| r)
        .or_else(|e| Err(e).context(format!("Failed to {}", action)))
}

impl From<Result<CommandResponce>> for AppResponse {
    fn from(result: Result<CommandResponce>) -> Self {
        match result {
            Ok(network_response) => Self::Network(network_response),
            Err(err) => Self::Error(err),
        }
    }
}

impl IntoResponse for AppResponse {
    fn into_response(self) -> Response {
        match self {
            AppResponse::Error(err) => {
                let errors: Vec<String> = err.chain().map(|e| format!("{}", e)).collect();
                let app_errors = AppErrors::new(errors);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(app_errors)).into_response()
            }
            AppResponse::Network(network_response) => match network_response {
                CommandResponce::ListConnections(connections) => {
                    (StatusCode::OK, Json(connections)).into_response()
                }
                CommandResponce::CheckConnectivity(connectivity) => {
                    (StatusCode::OK, Json(connectivity)).into_response()
                }
                CommandResponce::ListWiFiNetworks(networks) => {
                    (StatusCode::OK, Json(networks)).into_response()
                }
                CommandResponce::Shutdown(shutdown) => {
                    (StatusCode::OK, Json(shutdown)).into_response()
                }
                CommandResponce::Stop(stop) => (StatusCode::OK, Json(stop)).into_response(),
            },
        }
    }
}
