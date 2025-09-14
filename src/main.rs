use axum::{
    Router,
    body::Body,
    extract::{ConnectInfo, Path, Query, Request},
    http::{HeaderMap, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use js_cgi::scripts::{ServerRequest, execute_server_scripts};
use std::{
    collections::HashMap,
    fs,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
};
use tokio::net::TcpListener;
use tower::ServiceBuilder;

fn headers_to_map(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.to_string(), v.to_string()))
        })
        .collect()
}

fn get_local_ip() -> Option<String> {
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(local_addr) = socket.local_addr() {
                return Some(local_addr.ip().to_string());
            }
        }
    }

    Some("127.0.0.1".to_string())
}

fn serve_static_file(full_path: &PathBuf, content: &str) -> Response {
    let content_type = match full_path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("pdf") => "application/pdf",
        _ => "text/plain; charset=utf-8",
    };

    Response::builder()
        .header("content-type", content_type)
        .header("content-length", content.len())
        .body(Body::from(content.to_string()))
        .unwrap()
}

async fn build_server_request(
    method: Method,
    url: &str,
    headers: HeaderMap,
    query_params: HashMap<String, String>,
    client_addr: SocketAddr,
    request: Request,
) -> ServerRequest {
    let protocol = match request.version() {
        axum::http::Version::HTTP_09 => "HTTP/0.9",
        axum::http::Version::HTTP_10 => "HTTP/1.0",
        axum::http::Version::HTTP_11 => "HTTP/1.1",
        axum::http::Version::HTTP_2 => "HTTP/2.0",
        axum::http::Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    }
    .to_string();

    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(_) => String::new(),
    };

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(body.len());

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();

    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:3000");

    let (server_name, server_port) = if let Some((name, port)) = host.split_once(':') {
        (name.to_string(), port.parse().unwrap_or(80))
    } else {
        (host.to_string(), 80)
    };

    let is_https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "https")
        .unwrap_or(server_port == 443);

    let server_user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "www-data".to_string());

    let server_home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/var/www".to_string());

    let server_addr = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    ServerRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers: headers_to_map(&headers),
        query: query_params,
        body,
        server_name,
        server_port,
        remote_addr: client_addr.ip().to_string(),
        remote_port: client_addr.port(),
        is_https,
        protocol,
        content_length,
        content_type,
        server_user,
        server_home,
        server_addr,
    }
}

async fn serve_file(
    Path(file_path): Path<String>,
    method: Method,
    headers: HeaderMap,
    Query(query_params): Query<HashMap<String, String>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
) -> impl IntoResponse {
    println!("Serving file: {} Method: {}", file_path, method);

    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "./public".to_string());
    let full_path = PathBuf::from(static_dir.clone()).join(&file_path);

    if !full_path.starts_with(static_dir) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }

    let content = match fs::read_to_string(&full_path) {
        Ok(content) => content,
        Err(_) => return (StatusCode::NOT_FOUND, "Not Found").into_response(),
    };

    if content
        .to_lowercase()
        .trim_start()
        .starts_with("<!doctype server>")
    {
        let server_request = build_server_request(
            method,
            &format!("/{}", file_path),
            headers,
            query_params,
            addr,
            request,
        )
        .await;

        match execute_server_scripts(&content, &server_request) {
            Ok(processed_html) => Html(processed_html).into_response(),
            Err(e) => {
                eprintln!("Server script execution error: {:?}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Server Js Error").into_response()
            }
        }
    } else {
        serve_static_file(&full_path, &content)
    }
}

async fn serve_index() -> impl IntoResponse {
    Html(
        r#"
<!DOCTYPE html>
<html>
<head>
    <title>CGI-JS Server</title>
</head>
<body>
    <h1>CGI-JS Server is running!</h1>
    <p>Create HTML files with <code>&lt;script server&gt;</code> blocks to run server-side JavaScript.</p>
    <h2>Example:</h2>
    <pre><code>&lt;!DOCTYPE server&gt;
&lt;html&gt;
&lt;body&gt;
    &lt;h1&gt;Hello from server!&lt;/h1&gt;
    &lt;script server&gt;
        write('&lt;p&gt;Current time: ' + Date.now() + '&lt;/p&gt;');
        write('&lt;p&gt;Request method: ' + REQUEST.method + '&lt;/p&gt;');
    &lt;/script&gt;
&lt;/body&gt;
&lt;/html&gt;</code></pre>
</body>
</html>
    "#,
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/", get(serve_index))
        .route("/*file_path", get(serve_file))
        .layer(
            ServiceBuilder::new().layer(tower_http::trace::TraceLayer::new_for_http()), // 添加ConnectInfo中间件获取客户端地址
                                                                                        // .layer(axum::extract::connect_info::ConnectInfo::<SocketAddr>),
        );

    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse::<u16>()
        .unwrap_or(3000);

    let listener = TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    println!("CGI-JS server listening on http://localhost:{}", port);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
