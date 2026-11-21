use message_board::{api, store::Store};

#[tokio::main]
async fn main() {
    let db = std::env::var("MB_DB").unwrap_or_else(|_| "message-board.db".into());
    let addr = std::env::var("MB_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let store = Store::open(&db).expect("failed to open database");
    let app = api::router(api::AppState::new(store));
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind");
    println!("message board listening on http://{addr} (db: {db})");
    axum::serve(listener, app).await.expect("server error");
}
