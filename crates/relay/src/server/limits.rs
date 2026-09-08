//! Applies finite budgets before request parsing and response delivery.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use axum::{
    body::{to_bytes, Body},
    extract::{MatchedPath, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::config::Limits;

#[derive(Clone, Debug)]
struct Ingress {
    limits: Limits,
    requests: Arc<Semaphore>,
    rate: Arc<Mutex<Window>>,
}

#[derive(Debug)]
struct Window {
    started: Instant,
    used: u32,
}

impl Window {
    fn take(&mut self, now: Instant, limits: &Limits) -> bool {
        if now.duration_since(self.started) >= limits.rate_window {
            self.started = now;
            self.used = 0;
        }
        if self.used >= limits.requests_per_window {
            return false;
        }
        self.used += 1;
        true
    }
}

pub(crate) fn apply(router: Router, limits: &Limits) -> Router {
    let state = Ingress {
        limits: limits.clone(),
        requests: Arc::new(Semaphore::new(limits.connections)),
        rate: Arc::new(Mutex::new(Window {
            started: Instant::now(),
            used: 0,
        })),
    };
    router.layer(middleware::from_fn_with_state(state, bounded))
}

async fn bounded(State(state): State<Ingress>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let request_id = Uuid::now_v7().hyphenated().to_string();
    // Matched route templates contain no query strings or attacker-provided IDs.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let method = request.method().clone();
    tracing::info!(name: "relay.http.request", request_id, http.method = %method, http.route = route, "request received");
    let mut response = execute(&state, request, next).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
    );
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).expect("generated UUID header"),
    );
    tracing::info!(name: "relay.http.response", request_id, http.status_code = response.status().as_u16(), duration_ms = started.elapsed().as_millis(), "request completed");
    response
}

async fn execute(state: &Ingress, request: Request, next: Next) -> Response {
    let Ok(_permit) = state.requests.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let permitted = state
        .rate
        .lock()
        .is_ok_and(|mut rate| rate.take(Instant::now(), &state.limits));
    if !permitted {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match tokio::time::timeout(state.limits.request_timeout, async {
        let (parts, body) = request.into_parts();
        let body = match to_bytes(body, state.limits.body_bytes).await {
            Ok(body) => body,
            Err(_error) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        };
        let response = next.run(Request::from_parts(parts, Body::from(body))).await;
        let (parts, body) = response.into_parts();
        match to_bytes(body, state.limits.response_bytes).await {
            Ok(body) => Response::from_parts(parts, Body::from(body)),
            Err(_error) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    })
    .await
    {
        Ok(response) => response,
        Err(_error) => StatusCode::REQUEST_TIMEOUT.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::Request, routing::post};
    use std::time::Duration;
    use tower::ServiceExt;

    fn limits() -> Limits {
        crate::config::fixture_limits()
    }

    #[tokio::test]
    async fn body_limit_runs_before_the_handler_and_response_limit_before_delivery() {
        let app = apply(
            Router::new().route("/echo", post(|body: String| async move { body })),
            &limits(),
        );
        let response = app
            .clone()
            .oneshot(
                Request::post("/echo")
                    .body(Body::from("123456789"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let response = app
            .oneshot(
                Request::post("/echo")
                    .body(Body::from("ok"))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let app = apply(
            Router::new().route("/large", post(|| async { "response-larger-than-budget" })),
            &limits(),
        );
        let response = app
            .oneshot(
                Request::post("/large")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(to_bytes(response.into_body(), 16)
            .await
            .expect("bounded body")
            .is_empty());
    }

    #[tokio::test]
    async fn request_timeout_cancels_work_and_releases_capacity() {
        let app = apply(
            Router::new()
                .route(
                    "/slow",
                    post(|| async {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        StatusCode::NO_CONTENT
                    }),
                )
                .route("/fast", post(|| async { StatusCode::NO_CONTENT })),
            &limits(),
        );
        let response = app
            .clone()
            .oneshot(Request::post("/slow").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        let response = app
            .oneshot(Request::post("/fast").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn rate_budget_has_a_bounded_window_without_client_key_growth() {
        let now = Instant::now();
        let limits = limits();
        let mut window = Window {
            started: now,
            used: 0,
        };
        assert!(window.take(now, &limits));
        assert!(window.take(now, &limits));
        assert!(!window.take(now, &limits));
        assert!(window.take(now + limits.rate_window, &limits));
    }
}
