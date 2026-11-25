//! Automatic image updates through the API: a person opts a service in with a window through a
//! plan, the service then says so with what the updater last did, and a window in a time zone
//! Zeabur does not know is refused before anything is written.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use zeabur_kube::NamespaceObjects;
    use zeabur_kube::k8s_openapi::api::apps::v1::Deployment;
    use zeabur_kube::k8s_openapi::api::core::v1::Namespace;
    use zeabur_kube::memory::{MemoryCluster, State};

    use crate::common::{app, plan_and_apply, planned_and_applied, send};

    fn cluster() -> MemoryCluster {
        let namespace: Namespace = serde_json::from_value(json!({"metadata": {"name": "shop",
            "labels": {"zeabur.com/project": "true"}}}))
        .unwrap();
        let cache: Deployment = serde_json::from_value(json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "cache", "namespace": "shop", "uid": "u1",
                "resourceVersion": "41", "generation": 2},
            "spec": {"selector": {"matchLabels": {"app": "cache"}},
                "template": {"metadata": {"labels": {"app": "cache"}},
                    "spec": {"containers": [{"name": "redis", "image": "redis:7.2.4"}]}}}
        }))
        .unwrap();
        MemoryCluster::new(State {
            namespaces: vec![namespace],
            objects: BTreeMap::from([(
                "shop".to_owned(),
                NamespaceObjects {
                    namespace: "shop".to_owned(),
                    deployments: vec![cache],
                    ..NamespaceObjects::default()
                },
            )]),
            ..State::default()
        })
    }

    fn opt_in(window: &str) -> Value {
        json!([{"op": "imageUpdates", "workload": {"kind": "Deployment", "name": "cache"},
            "window": window}])
    }

    async fn service(app: &axum::Router) -> Value {
        let (status, body) = send(app, "GET", "/api/v1/projects/shop", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["services"][0].clone()
    }

    #[tokio::test]
    async fn a_service_opted_in_says_its_window_and_one_opted_out_does_not() {
        let cluster = cluster();
        let app = app(&cluster);
        assert_eq!(service(&app).await.get("imageUpdates"), None);
        planned_and_applied(&app, "shop", opt_in("sun 03:00 2h Europe/Amsterdam")).await;
        assert_eq!(
            service(&app).await["imageUpdates"],
            json!({"window": "sun 03:00 2h Europe/Amsterdam", "last": {}})
        );
        planned_and_applied(
            &app,
            "shop",
            json!([{"op": "imageUpdates", "workload": {"kind": "Deployment", "name": "cache"}}]),
        )
        .await;
        assert_eq!(service(&app).await.get("imageUpdates"), None);
    }

    #[tokio::test]
    async fn a_window_in_an_unknown_time_zone_is_refused() {
        let cluster = cluster();
        let (status, body) =
            plan_and_apply(&app(&cluster), "shop", opt_in("sun 03:00 2h Mars/Olympus")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            body["detail"],
            "Mars/Olympus is not a time zone Zeabur knows."
        );
        assert_eq!(cluster.inspect(|s| s.writes.len()), Some(0));
    }
}
