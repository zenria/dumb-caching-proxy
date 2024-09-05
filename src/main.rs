use std::{sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::HeaderMap,
    middleware::from_fn,
    routing::get,
    Router,
};
use clap::Parser;
use jemallocator::Jemalloc;
use moka::future::Cache;
use reqwest::{Method, RequestBuilder, StatusCode, Url};
use serde::Deserialize;
use service_helpe_rs::axum::{metrics::metrics_middleware, tracing_access_log::access_log};
use sha2::{Digest, Sha256};
use tower::ServiceBuilder;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing::{debug, error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

/// Dump caching proxy
#[derive(Parser, Debug)]
struct Opt {
    /// bind address, please do not expose this to the public internet
    #[arg(long, env, default_value = "127.0.0.1:8666")]
    bind_address: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let opt = Opt::parse();
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    tracing::info!("Starting dumb-caching-proxy with config: {opt:?}");

    let middleware = ServiceBuilder::new()
        .layer(from_fn(metrics_middleware))
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::SERVER,
            axum::http::HeaderValue::from_str(&format!("dumb-caching-proxy (axum/0.7)",))?,
        ))
        .layer(from_fn(access_log));

    let cache: Arc<Cache<Vec<u8>, (HeaderMap, Bytes)>> =
        Arc::new(Cache::builder().initial_capacity(10).build());

    // build our application with a route
    let app = Router::new()
        .route("/proxy", get(proxy))
        .route("/proxy-cache", get(proxy_cache))
        .with_state(cache)
        .layer(middleware);

    tracing::info!("Listening on {}", opt.bind_address);

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind(&opt.bind_address)
        .await
        .with_context(|| format!("Unable to listen on {}", opt.bind_address))?;

    axum::serve(listener, app).await.unwrap();

    Ok(())
}

#[derive(Debug, Deserialize)]
struct ProxyParams {
    url: String,
    method: Option<String>,
}

/// if any error occurs while fetching upstream (timeout, dns issue, not a 200 response),
/// return last valid response.
async fn proxy_cache(
    Query(params): Query<ProxyParams>,
    headers: HeaderMap,
    State(cache): State<Arc<Cache<Vec<u8>, (HeaderMap, Bytes)>>>,
    body: Bytes,
) -> Result<(StatusCode, HeaderMap, Bytes), (axum::http::StatusCode, &'static str)> {
    let cache_key = Sha256::new()
        .chain_update(params.method.as_ref().map(String::as_str).unwrap_or("GET"))
        .chain_update(&params.url)
        .finalize()
        .to_vec();

    match proxy(Query(params), headers, body).await {
        Ok((status, headers, body)) => {
            if status != StatusCode::OK {
                // let try to get response from cache
                let cached_response = cache.get(&cache_key).await;
                info!(
                    ?status,
                    "Error from upstream, trying to serve cached response",
                );
                match cached_response {
                    Some((header, body)) => Ok((StatusCode::OK, header, body)),
                    //something bad happened, propagate response
                    None => {
                        error!("Cache is empty");
                        Ok((status, headers, body))
                    }
                }
            } else {
                cache
                    .insert(cache_key, (headers.clone(), body.clone()))
                    .await;
                Ok((StatusCode::OK, headers, body))
            }
        }
        Err(e) => {
            // let try to get response from cache
            info!("Error from upstream, trying to serve cached response",);
            let cached_response = cache.get(&cache_key).await;
            match cached_response {
                Some((header, body)) => Ok((StatusCode::OK, header, body)),
                None => {
                    error!("Cache is empty");
                    Err(e)
                }
            }
        }
    }
}

/// simple proxy
async fn proxy(
    Query(params): Query<ProxyParams>,
    mut headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, HeaderMap, Bytes), (axum::http::StatusCode, &'static str)> {
    tracing::info!(?params.url, ?params.method, ?headers);

    let Ok(method) = params
        .method
        .as_ref()
        .map(String::as_bytes)
        .map(Method::from_bytes)
        .transpose()
    else {
        return Err((StatusCode::BAD_REQUEST, "invalid method"));
    };
    let Ok(url) = Url::parse(&params.url) else {
        return Err((StatusCode::BAD_REQUEST, "invalid url"));
    };

    headers.remove("host");
    headers.remove("accept-encoding"); // we just want plain text no fancy compression

    debug!(?url, ?headers);

    get_response(
        reqwest::Client::new()
            .request(method.unwrap_or(Method::GET), url)
            .headers(headers)
            // 10 secs seems more than enough
            .timeout(Duration::from_secs(10))
            .body(body),
    )
    .await
    .map_err(|error| {
        tracing::error!(?error);
        if error.is_timeout() {
            (StatusCode::GATEWAY_TIMEOUT, "Gateway Timeout")
        } else {
            (StatusCode::BAD_GATEWAY, "Bad Gateway")
        }
    })
    .map(|(status, mut headers, body)| {
        // remove problematic headers, those will be set bu axum/hyper
        headers.remove("transfer-encoding");
        headers.remove("content-length");
        (status, headers, body)
    })
}

async fn get_response(
    request_builder: RequestBuilder,
) -> Result<(StatusCode, HeaderMap, Bytes), reqwest::Error> {
    let response = request_builder.send().await?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.bytes().await?;

    debug!(?status, ?headers, ?body);

    Ok((status, headers, body))
}
