use axum::{Router, extract::Path, routing::get};
use js_cgi::route::serve_file;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower::ServiceBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route(
            "/",
            get(|m, h, q, c, r| serve_file(Path("index.html".to_string()), m, h, q, c, r)),
        )
        .fallback(get(serve_file))
        .layer(ServiceBuilder::new().layer(tower_http::trace::TraceLayer::new_for_http()));

    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse::<u16>()
        .unwrap_or(3000);

    let listener = TcpListener::bind(&format!("0.0.0.0:{}", port)).await?;
    println!("CGI-JS server listening on http://localhost:{}", port);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
