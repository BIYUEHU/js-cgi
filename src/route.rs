use crate::scripts::{ServerRequest, execute_server_scripts};
use axum::{
    body::Body,
    extract::{ConnectInfo, Path, Query, Request},
    http::{HeaderMap, Method, StatusCode},
    response::{Html, IntoResponse},
};
use std::{
    collections::HashMap,
    fs,
    net::{SocketAddr, UdpSocket},
    path::PathBuf,
};
use tower::util::ServiceExt;

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

pub async fn serve_file(
    Path(file_path): Path<String>,
    method: Method,
    headers: HeaderMap,
    Query(query_params): Query<HashMap<String, String>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
) -> impl IntoResponse {
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "./public".to_string());
    let mut path_buf = PathBuf::from(&static_dir).join(&file_path);

    if file_path.ends_with('/') || file_path.is_empty() {
        path_buf.push("index.html");
    }

    let canonical_static =
        fs::canonicalize(&static_dir).unwrap_or_else(|_| PathBuf::from(&static_dir));
    let canonical_full = match fs::canonicalize(&path_buf) {
        Ok(p) => p,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    if !canonical_full.starts_with(canonical_static) {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }

    let extension = path_buf.extension().and_then(|s| s.to_str()).unwrap_or("");

    if extension == "html" || extension == "htm" {
        match fs::read_to_string(&path_buf) {
            Ok(content) => {
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
                    // 普通 HTML 没带标志位，直接作为 HTML 返回
                    Html(content).into_response()
                }
            }
            Err(_) => StatusCode::NOT_FOUND.into_response(),
        }
    } else {
        // 4. 非 HTML 资源（图片、JS、CSS 等）
        // 别用 read_to_string！用专业的 ServeFile，支持 Range 请求和高效流式传输
        match tower_http::services::fs::ServeFile::new(&path_buf)
            .oneshot(Request::new(Body::empty()))
            .await
        {
            Ok(res) => res.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}
