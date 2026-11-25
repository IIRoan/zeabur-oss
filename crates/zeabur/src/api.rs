//! The API, called in this process as the pod's service account, as `zeabur components install`
//! and `zeabur image-updates` act.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, header};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tower::ServiceExt as _;
use zeabur_api::access::SingleCluster;
use zeabur_core::problem::Problem;
use zeabur_kube::Cluster;

/// The largest answer read.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// The API, called in this process as the service account.
pub(crate) struct Api {
    router: Router,
}

impl Api {
    pub(crate) fn new(cluster: Arc<dyn Cluster>, settings: zeabur_api::Settings) -> Self {
        Self {
            router: zeabur_api::router_with(
                Arc::new(SingleCluster(cluster)),
                zeabur_api::Options {
                    settings,
                    ..zeabur_api::Options::default()
                },
            ),
        }
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Box<Problem>> {
        self.send(Request::builder().uri(path), Body::empty()).await
    }

    pub(crate) async fn post<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, Box<Problem>> {
        let bytes = serde_json::to_vec(body).map_err(internal)?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, bytes.len());
        self.send(request, Body::from(bytes)).await
    }

    async fn send<T: DeserializeOwned>(
        &self,
        request: axum::http::request::Builder,
        body: Body,
    ) -> Result<T, Box<Problem>> {
        let request = request.body(body).map_err(internal)?;
        let Ok(response) = self.router.clone().oneshot(request).await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), MAX_BODY)
            .await
            .map_err(internal)?;
        if status.is_success() {
            serde_json::from_slice(&body).map_err(internal)
        } else {
            Err(serde_json::from_slice(&body).map_or_else(
                |_e| internal(format!("the API answered {status} without a problem")),
                Box::new,
            ))
        }
    }
}

fn internal(error: impl std::fmt::Display) -> Box<Problem> {
    Box::new(Problem::new(
        zeabur_core::problem::Kind::Internal,
        error.to_string(),
    ))
}
