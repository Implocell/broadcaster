use axum::{Router, http::StatusCode, response::IntoResponse, routing::get};

#[tokio::main]
async fn main() {
    // build our application with a route
    let app = Router::new().route("/health", get(health_handler));
    // run it
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn health_handler() -> impl IntoResponse {
    StatusCode::OK
}
