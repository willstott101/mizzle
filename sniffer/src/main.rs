use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let client = reqwest::Client::builder().build()?;

    let app = Router::new().fallback(any(proxy)).with_state(client);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn proxy(
    State(client): State<reqwest::Client>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let git_protocol = headers
        .get("Git-Protocol")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("version=2");
    if git_protocol != "version=2" {
        tracing::warn!("rejecting non-v2 Git-Protocol: {git_protocol}");
        return (
            StatusCode::NOT_IMPLEMENTED,
            "Only Git Protocol 2 is supported",
        )
            .into_response();
    }

    let path_and_query = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("https://github.com{path_and_query}");
    tracing::info!("REQ {method} {url}");

    if method == Method::POST && !body.is_empty() {
        tracing::info!("POST BODY\n{}", String::from_utf8_lossy(&body));
    }

    let mut req = client.request(method, &url);

    // Forward the headers git relies on for smart-http protocol negotiation.
    for name in ["Content-Type", "Git-Protocol", "Accept"] {
        if let Some(v) = headers.get(name) {
            req = req.header(name, v);
        }
    }
    if !body.is_empty() {
        req = req.body(body);
    }

    let upstream = match req.send().await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!("upstream request failed: {err}");
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {err}")).into_response();
        }
    };

    let status = upstream.status();
    let content_type = upstream.headers().get("Content-Type").cloned();
    let resp_body = match upstream.bytes().await {
        Ok(b) => b,
        Err(err) => {
            tracing::error!("reading upstream body failed: {err}");
            return (
                StatusCode::BAD_GATEWAY,
                format!("upstream body error: {err}"),
            )
                .into_response();
        }
    };

    tracing::info!("RESPONSE BODY\n{:?}", resp_body);

    let mut response = (status, resp_body).into_response();
    if let Some(ct) = content_type {
        response.headers_mut().insert("Content-Type", ct);
    }
    response
}
