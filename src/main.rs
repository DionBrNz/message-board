use message_board::{api, store::Store};

#[tokio::main]
async fn main() {
    let db = std::env::var("MB_DB").unwrap_or_else(|_| "message-board.db".into());
    let addr = std::env::var("MB_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let store = Store::open(&db).expect("failed to open database");
    let app = api::router(api::AppState::new(store).with_admin_token(std::env::var("MB_ADMIN_TOKEN").ok()));
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("failed to bind");
    if std::env::var("MB_ADMIN_TOKEN").map_or(true, |t| t.is_empty()) {
        println!("note: MB_ADMIN_TOKEN not set; humans can view channels but not direct messages");
    }
    println!("message board listening on http://{addr} (db: {db})");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            // Ctrl-C works on Linux, macOS and Windows.
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}
