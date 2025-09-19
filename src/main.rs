use axum::{
    Router,
    extract::{WebSocketUpgrade, ws::WebSocket},
    http::StatusCode,
    response::Response,
    routing::get,
};
use dashmap::DashMap;
use futures_util::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc,
    time::{interval, timeout},
};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

// Maximum message size (64KB)
const MAX_MESSAGE_SIZE: usize = 64 * 1024;
// Rate limiting: minimum interval between messages
const RATE_LIMIT_INTERVAL: Duration = Duration::from_millis(100);
// Room TTL when empty (10 minutes)
const ROOM_TTL: Duration = Duration::from_secs(600);

/// Message sent by client to join a broadcast room
#[derive(Debug, Deserialize)]
struct JoinMessage {
    broadcastid: String,
    username: String,
}

/// Standard chat message structure
#[derive(Debug, Deserialize, Serialize, Clone)]
struct ChatMessage {
    username: String,
    broadcastid: String,
    payload: serde_json::Value,
}

/// Internal message for broadcasting
#[derive(Debug, Clone)]
struct InternalMessage {
    message: ChatMessage,
    sender_id: String, // To avoid echoing back to sender
}

/// User connection information
#[derive(Debug)]
struct UserConnection {
    sender: mpsc::UnboundedSender<axum::extract::ws::Message>,
    last_message_time: Arc<tokio::sync::Mutex<Instant>>,
    user_id: String,
}

/// Broadcast room information
#[derive(Debug)]
struct BroadcastRoom {
    users: HashMap<String, UserConnection>, // username -> connection
    created_at: Instant,
    last_activity: Instant,
    broadcast_sender: tokio::sync::broadcast::Sender<InternalMessage>,
}

impl BroadcastRoom {
    fn new() -> Self {
        let (broadcast_sender, _) = tokio::sync::broadcast::channel(1000);
        let now = Instant::now();

        Self {
            users: HashMap::new(),
            created_at: now,
            last_activity: now,
            broadcast_sender,
        }
    }

    fn update_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    fn is_expired(&self) -> bool {
        self.users.is_empty() && self.last_activity.elapsed() > ROOM_TTL
    }
}

/// Global server state
#[derive(Clone)]
struct ServerState {
    rooms: Arc<DashMap<String, BroadcastRoom>>,
    shutdown: Arc<AtomicBool>,
}

impl ServerState {
    fn new() -> Self {
        Self {
            rooms: Arc::new(DashMap::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Health check endpoint
async fn health_check() -> StatusCode {
    StatusCode::OK
}

/// WebSocket upgrade endpoint
async fn websocket_handler(ws: WebSocketUpgrade, state: ServerState) -> Response {
    ws.on_upgrade(move |socket| handle_websocket(socket, state))
}

/// Main WebSocket connection handler
async fn handle_websocket(socket: WebSocket, state: ServerState) {
    let (mut sender, mut receiver) = socket.split();
    let user_id = uuid::Uuid::new_v4().to_string();

    info!("New WebSocket connection established: {}", user_id);

    // Wait for the first message (join message)
    let join_message = match receive_join_message(&mut receiver).await {
        Ok(msg) => msg,
        Err(e) => {
            error!("Failed to receive join message from {}: {}", user_id, e);
            let _ = sender
                .send(axum::extract::ws::Message::Close(Some(
                    axum::extract::ws::CloseFrame {
                        code: 1000,
                        reason: "Invalid join message".into(),
                    },
                )))
                .await;
            return;
        }
    };

    info!(
        "User {} attempting to join broadcast: {}",
        join_message.username, join_message.broadcastid
    );

    // Create message channel for this connection
    let (tx, mut rx) = mpsc::unbounded_channel();
    let last_message_time = Arc::new(tokio::sync::Mutex::new(Instant::now()));

    let tx_clone = tx.clone();

    // Try to join the room
    let broadcast_receiver = match join_room(
        &state,
        &join_message.broadcastid,
        &join_message.username,
        UserConnection {
            sender: tx,
            last_message_time: last_message_time.clone(),
            user_id: user_id.clone(),
        },
    )
    .await
    {
        Ok(receiver) => receiver,
        Err(e) => {
            error!(
                "Failed to join room {} for user {}: {}",
                join_message.broadcastid, join_message.username, e
            );
            let _ = sender
                .send(axum::extract::ws::Message::Close(Some(
                    axum::extract::ws::CloseFrame {
                        code: 1000,
                        reason: e.into(),
                    },
                )))
                .await;
            return;
        }
    };

    info!(
        "User {} successfully joined broadcast: {}",
        join_message.username, join_message.broadcastid
    );

    // Send join notification to other users in the room
    if let Err(e) = broadcast_system_message(
        &state,
        &join_message.broadcastid,
        &join_message.username,
        serde_json::json!({
            "type": "user_joined",
            "message": format!("{} joined the chat", join_message.username)
        }),
        &user_id,
    )
    .await
    {
        warn!("Failed to send join notification: {}", e);
    }

    // Clone necessary data for tasks
    let broadcast_id = join_message.broadcastid.clone();
    let username = join_message.username.clone();
    let state_clone = state.clone();
    let user_id_for_send = user_id.clone();
    let user_id_for_cleanup = user_id.clone();

    // Task to handle incoming WebSocket messages
    let receive_task = {
        let state = state.clone();
        let broadcast_id = broadcast_id.clone();
        let username = username.clone();
        let last_message_time = last_message_time.clone();

        tokio::spawn(async move {
            handle_incoming_messages(
                receiver,
                state,
                broadcast_id,
                username,
                user_id,
                last_message_time,
            )
            .await;
        })
    };

    // Task to handle outgoing messages (from broadcast channel)
    let send_task = {
        let mut broadcast_receiver = broadcast_receiver;

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Receive messages from the room's broadcast channel
                    msg = broadcast_receiver.recv() => {
                        match msg {
                            Ok(internal_msg) => {
                                // Don't echo back to sender
                                if internal_msg.sender_id == user_id_for_send {
                                    continue;
                                }

                                if let Ok(json) = serde_json::to_string(&internal_msg.message) {
                                    if tx_clone.send(axum::extract::ws::Message::Text(json.into())).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(_) => break, // Channel closed
                        }
                    }
                    // Send queued messages to WebSocket
                    msg = rx.recv() => {
                        match msg {
                            Some(message) => {
                                if sender.send(message).await.is_err() {
                                    break;
                                }
                            }
                            None => break, // Channel closed
                        }
                    }
                }
            }
        })
    };

    // Wait for either task to complete (connection closed)
    tokio::select! {
        _ = receive_task => {},
        _ = send_task => {},
    }

    // Cleanup when connection closes
    leave_room(&state_clone, &broadcast_id, &username).await;

    // Send leave notification to other users
    if let Err(e) = broadcast_system_message(
        &state_clone,
        &broadcast_id,
        &username,
        serde_json::json!({
            "type": "user_left",
            "message": format!("{} left the chat", username)
        }),
        &user_id_for_cleanup,
    )
    .await
    {
        warn!("Failed to send leave notification: {}", e);
    }

    info!(
        "User {} disconnected from broadcast: {}",
        username, broadcast_id
    );
}

/// Receive and validate the initial join message
async fn receive_join_message(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<JoinMessage, String> {
    let timeout_duration = Duration::from_secs(30); // 30 second timeout for join message

    let message = timeout(timeout_duration, receiver.next())
        .await
        .map_err(|_| "Timeout waiting for join message")?
        .ok_or("Connection closed before join message")?
        .map_err(|e| format!("WebSocket error: {}", e))?;

    let text = match message {
        axum::extract::ws::Message::Text(text) => {
            if text.len() > MAX_MESSAGE_SIZE {
                return Err("Join message too large".to_string());
            }
            text
        }
        _ => return Err("Expected text message for join".to_string()),
    };

    serde_json::from_str::<JoinMessage>(&text)
        .map_err(|e| format!("Invalid join message format: {}", e))
}

/// Handle incoming messages from a WebSocket connection
async fn handle_incoming_messages(
    mut receiver: futures_util::stream::SplitStream<WebSocket>,
    state: ServerState,
    broadcast_id: String,
    username: String,
    user_id: String,
    last_message_time: Arc<tokio::sync::Mutex<Instant>>,
) {
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(axum::extract::ws::Message::Text(text)) => {
                if text.len() > MAX_MESSAGE_SIZE {
                    warn!(
                        "Message too large from user {}: {} bytes",
                        username,
                        text.len()
                    );
                    continue;
                }

                // Rate limiting check
                {
                    let mut last_time = last_message_time.lock().await;
                    let now = Instant::now();
                    if now.duration_since(*last_time) < RATE_LIMIT_INTERVAL {
                        warn!("Rate limit exceeded for user {}", username);
                        continue;
                    }
                    *last_time = now;
                }

                // Parse and validate chat message
                let chat_message: ChatMessage = match serde_json::from_str(&text) {
                    Ok(msg) => msg,
                    Err(e) => {
                        warn!("Invalid message format from user {}: {}", username, e);
                        continue;
                    }
                };

                // Validate message fields
                if chat_message.username != username || chat_message.broadcastid != broadcast_id {
                    warn!(
                        "Message validation failed for user {} in broadcast {}: username={}, broadcastid={}",
                        username, broadcast_id, chat_message.username, chat_message.broadcastid
                    );
                    continue;
                }

                // Broadcast the message to the room
                if let Err(e) =
                    broadcast_message(&state, &broadcast_id, chat_message, &user_id).await
                {
                    error!("Failed to broadcast message: {}", e);
                }
            }
            Ok(axum::extract::ws::Message::Close(_)) => {
                info!("WebSocket closed by client: {}", username);
                break;
            }
            Ok(axum::extract::ws::Message::Ping(data)) => {
                // Respond to ping with pong (handled automatically by axum)
                info!("Received ping from {}", username);
            }
            Ok(_) => {
                // Ignore other message types (binary, pong)
            }
            Err(e) => {
                error!("WebSocket error for user {}: {}", username, e);
                break;
            }
        }
    }
}

/// Join a user to a broadcast room
async fn join_room(
    state: &ServerState,
    broadcast_id: &str,
    username: &str,
    connection: UserConnection,
) -> Result<tokio::sync::broadcast::Receiver<InternalMessage>, String> {
    // Validate input
    if broadcast_id.is_empty() || username.is_empty() {
        return Err("Broadcast ID and username cannot be empty".to_string());
    }

    if broadcast_id.len() > 100 || username.len() > 50 {
        return Err("Broadcast ID or username too long".to_string());
    }

    // Get or create room
    let mut room = state
        .rooms
        .entry(broadcast_id.to_string())
        .or_insert_with(|| {
            info!("Creating new broadcast room: {}", broadcast_id);
            BroadcastRoom::new()
        });

    // Check for duplicate username
    if room.users.contains_key(username) {
        return Err(format!(
            "Username '{}' already exists in this broadcast",
            username
        ));
    }

    // Subscribe to room's broadcast channel
    let receiver = room.broadcast_sender.subscribe();

    // Add user to room
    room.users.insert(username.to_string(), connection);
    room.update_activity();

    info!(
        "User '{}' joined broadcast '{}' (total users: {})",
        username,
        broadcast_id,
        room.users.len()
    );

    Ok(receiver)
}

/// Remove a user from a broadcast room
async fn leave_room(state: &ServerState, broadcast_id: &str, username: &str) {
    if let Some(mut room) = state.rooms.get_mut(broadcast_id) {
        room.users.remove(username);
        room.update_activity();

        let user_count = room.users.len();
        info!(
            "User '{}' left broadcast '{}' (remaining users: {})",
            username, broadcast_id, user_count
        );

        // If room is empty, it will be cleaned up by the TTL cleanup task
    }
}

/// Broadcast a message to all users in a room
async fn broadcast_message(
    state: &ServerState,
    broadcast_id: &str,
    message: ChatMessage,
    sender_id: &str,
) -> Result<(), String> {
    if let Some(mut room) = state.rooms.get_mut(broadcast_id) {
        room.update_activity();

        let internal_msg = InternalMessage {
            message,
            sender_id: sender_id.to_string(),
        };

        if let Err(e) = room.broadcast_sender.send(internal_msg) {
            return Err(format!("Failed to broadcast message: {}", e));
        }
    } else {
        return Err("Broadcast room not found".to_string());
    }

    Ok(())
}

/// Broadcast a system message to all users in a room
async fn broadcast_system_message(
    state: &ServerState,
    broadcast_id: &str,
    from_username: &str,
    payload: serde_json::Value,
    sender_id: &str,
) -> Result<(), String> {
    let system_message = ChatMessage {
        username: "system".to_string(),
        broadcastid: broadcast_id.to_string(),
        payload,
    };

    broadcast_message(state, broadcast_id, system_message, sender_id).await
}

/// Background task to cleanup expired rooms
async fn cleanup_expired_rooms(state: ServerState) {
    let mut interval = interval(Duration::from_secs(60)); // Check every minute

    loop {
        interval.tick().await;

        if state.shutdown.load(Ordering::Relaxed) {
            break;
        }

        let expired_rooms: Vec<String> = state
            .rooms
            .iter()
            .filter_map(|entry| {
                let (key, room) = entry.pair();
                if room.is_expired() {
                    Some(key.clone())
                } else {
                    None
                }
            })
            .collect();

        for room_id in expired_rooms {
            state.rooms.remove(&room_id);
            info!("Cleaned up expired room: {}", room_id);
        }

        // Log current room statistics
        let total_rooms = state.rooms.len();
        let total_users: usize = state
            .rooms
            .iter()
            .map(|entry| entry.value().users.len())
            .sum();

        if total_rooms > 0 {
            info!(
                "Server stats - Active rooms: {}, Total users: {}",
                total_rooms, total_users
            );
        }
    }

    info!("Room cleanup task shutting down");
}

/// Setup graceful shutdown handler
async fn setup_graceful_shutdown(state: ServerState) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received, starting graceful shutdown...");
    state.shutdown.store(true, Ordering::Relaxed);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("websocket_chat_server=info".parse()?),
        )
        .init();

    let state = ServerState::new();

    // Start background cleanup task
    let cleanup_state = state.clone();
    tokio::spawn(cleanup_expired_rooms(cleanup_state));

    // Setup graceful shutdown
    let shutdown_state = state.clone();
    tokio::spawn(setup_graceful_shutdown(shutdown_state));

    // Build our application with routes
    let app = Router::new()
        .route("/health", get(health_check))
        .route(
            "/join",
            get({
                let state = state.clone();
                move |ws| websocket_handler(ws, state)
            }),
        )
        .layer(TraceLayer::new_for_http());

    info!("Starting WebSocket chat server on port 8080");
    info!("Health check available at: http://localhost:8080/health");
    info!("WebSocket endpoint available at: ws://localhost:8080/join");

    // Run the server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;

    info!("Server shut down gracefully");
    Ok(())
}
