//! `zeabur image-updates`: the image updater. The chart runs it in a Deployment of its own
//! (`imageUpdates.enabled`), as its own service account, which may read projects as a viewer and
//! patch Deployments and StatefulSets; an admission policy holds those patches to opted-in
//! workloads. Off the cluster it acts as the person of the current kubeconfig context.
//!
//! Every interval it lists the opted-in Deployments and StatefulSets (one list per kind, by the
//! label) and asks `zeabur_plan::image_update` about each one, read again just before:
//!
//! - **in its window**, the repository's tags, and the write that moves the image to the newest
//!   patch release and records the check;
//! - **while an update rolls out**, the service's rollout as the API reports it (`GET …/rollout`),
//!   and, when it is stuck or late, the rollback that report offers, planned and applied through
//!   the API (`Change::Rollback`);
//!
//! Each write is a JSON Patch whose `test`s refuse it when the workload changed since it was read;
//! the next interval reads it again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};
use zeabur_api::images::Inspector;
use zeabur_api::projects::writes::{ApplyRequest, ApplyResponse, PlanResponse};
use zeabur_kube::{
    Cluster, ClusterEndpoint, ObjectTarget, ResourceRef, UserClient, WriteBody, WriteRequest,
};
use zeabur_plan::image_update::{
    IMAGE_UPDATES, Outcome, PATCH, Rollout, State, follow, image_update, not_rolled_back,
    record_of, split_tag,
};

use crate::api::Api;

/// The service account token of this pod, which kubelet renews.
const TOKEN_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// The image updater.
#[derive(Debug, clap::Parser)]
#[command(version)]
pub(crate) struct ImageUpdatesArgs {
    /// How often the opted-in workloads are read, such as `30s` or `1m`.
    #[arg(long, default_value = "30s", value_parser = crate::components::duration)]
    interval: Duration,
    /// A PEM file of certificate authorities to trust, on top of the platform's roots, for
    /// registries.
    #[arg(long, env = "ZEABUR_EXTRA_CA_FILE")]
    extra_ca_file: Option<PathBuf>,
}

/// What one pass needs.
struct Updater {
    cluster: Arc<dyn Cluster>,
    api: Api,
    registries: Inspector,
}

pub(crate) async fn run(args: &ImageUpdatesArgs) -> Result<(), String> {
    let trusted_ca = crate::config::bundle(&[(&args.extra_ca_file, "--extra-ca-file")])?;
    let cluster: Arc<dyn Cluster> = Arc::new(
        match ClusterEndpoint::in_cluster() {
            Ok(endpoint) => UserClient::with_token_file(&endpoint, Path::new(TOKEN_FILE)),
            Err(_not_in_a_pod) => UserClient::from_kubeconfig(None).await,
        }
        .map_err(|e| e.to_string())?,
    );
    let settings = zeabur_api::Settings {
        trusted_ca: trusted_ca.clone(),
        template_fetch: false,
        ..zeabur_api::Settings::default()
    };
    let tls = zeabur_http::tls(trusted_ca.as_deref());
    let registries = Inspector::connected(zeabur_http::Reach::Public, &tls, trusted_ca);
    let updater = Updater {
        api: Api::new(Arc::clone(&cluster), settings),
        cluster,
        registries,
    };
    tracing::info!(event = "image_updates.start", interval = ?args.interval, "following opted-in workloads");
    let stop = crate::stop_signal();
    tokio::pin!(stop);
    loop {
        updater.pass().await;
        tokio::select! {
            () = &mut stop => return Ok(()),
            () = tokio::time::sleep(args.interval) => {}
        }
    }
}

/// The kinds that are updated.
fn kinds() -> [ResourceRef; 2] {
    [
        ResourceRef::new("apps", "v1", "deployments", "Deployment"),
        ResourceRef::new("apps", "v1", "statefulsets", "StatefulSet"),
    ]
}

impl Updater {
    /// One pass over every opted-in workload.
    async fn pass(&self) {
        let mut tags: HashMap<String, Result<Vec<String>, String>> = HashMap::new();
        for kind in kinds() {
            let found = match self
                .cluster
                .list_labelled(&kind, None, IMAGE_UPDATES, PATCH)
                .await
            {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(event = "image_updates.list", kind = %kind.kind, %error, "not listed");
                    continue;
                }
            };
            for meta in found {
                let (Some(namespace), Some(name)) = (meta.namespace, meta.name) else {
                    continue;
                };
                let at = Workload {
                    kind: &kind,
                    namespace: &namespace,
                    name: &name,
                };
                if let Err(error) = self.one(&at, &mut tags).await {
                    tracing::warn!(event = "image_updates.workload", namespace, name, %error, "not done");
                }
            }
        }
    }

    /// What is due for one workload: following its update's rollout, or checking for a newer
    /// patch release in its window.
    async fn one(
        &self,
        at: &Workload<'_>,
        tags: &mut HashMap<String, Result<Vec<String>, String>>,
    ) -> Result<(), String> {
        let workload = self.read(at).await?;
        let record = record_of(&workload);
        if record.state == Some(State::Updating) {
            return self.follow(at).await;
        }
        // Asked first without tags: only a workload due for a check has its registry read.
        if image_update(&workload, &[], &zone, Timestamp::now()).is_err() {
            return Ok(());
        }
        let image = image_of(&workload).ok_or("the workload names no image")?;
        let repository = split_tag(&image).map_or(image.as_str(), |(r, _)| r);
        let listed = if let Some(listed) = tags.get(repository) {
            listed.clone()
        } else {
            let listed = self.registries.tags(&image).await;
            tags.insert(repository.to_owned(), listed.clone());
            listed
        };
        let listed = listed.map_err(|why| format!("the tags of {repository}: {why}"))?;
        let workload = self.read(at).await?;
        let update =
            image_update(&workload, &listed, &zone, Timestamp::now()).map_err(|e| e.detail)?;
        self.patch(at, update.operations).await?;
        match &update.image {
            Some(image) => tracing::info!(
                event = "image_updates.update",
                namespace = at.namespace,
                name = at.name,
                image,
                "updating"
            ),
            None => {
                tracing::info!(event = "image_updates.check", namespace = at.namespace, name = at.name, awaiting = ?update.awaiting, "no newer patch release");
            }
        }
        Ok(())
    }

    /// Follows the rollout of the update recorded on the workload, and rolls it back when it fails.
    async fn follow(&self, at: &Workload<'_>) -> Result<(), String> {
        let path = format!(
            "/api/v1/projects/{}/services/{}/{}/rollout",
            at.namespace,
            at.kind.kind.to_ascii_lowercase(),
            at.name
        );
        let report: Report = self.api.get(&path).await.map_err(|p| p.detail)?;
        let rollout = match (report.status.as_str(), &report.reason) {
            ("done", _) => Rollout::Done,
            ("stuck", reason) => Rollout::Stuck(reason.clone().unwrap_or_default()),
            _ => Rollout::Progressing,
        };
        let workload = self.read(at).await?;
        let followed = follow(&workload, &rollout, Timestamp::now()).map_err(|e| e.detail)?;
        if !followed.operations.is_empty() {
            self.patch(at, followed.operations).await?;
        }
        tracing::info!(event = "image_updates.follow", namespace = at.namespace, name = at.name, outcome = ?followed.outcome, "followed");
        if followed.outcome != Outcome::RollBack {
            return Ok(());
        }
        let rolled = match report.rollback {
            Some(plan) => self.rollback(at.namespace, &plan).await,
            None => Err("the rollout report offers no release to go back to".to_owned()),
        };
        if let Err(why) = rolled {
            let workload = self.read(at).await?;
            self.patch(at, not_rolled_back(&workload, &why).map_err(|e| e.detail)?)
                .await?;
            return Err(format!("not rolled back: {why}"));
        }
        Ok(())
    }

    /// Plans and applies `plan` in `project` through the API, as a person's rollback is.
    async fn rollback(&self, project: &str, plan: &Value) -> Result<(), String> {
        let planned: PlanResponse = self
            .api
            .post(&format!("/api/v1/projects/{project}/plan"), plan)
            .await
            .map_err(|p| p.detail)?;
        let apply = ApplyRequest {
            token: planned.token,
            via: None,
        };
        let _applied: ApplyResponse = self
            .api
            .post(&format!("/api/v1/projects/{project}/apply"), &apply)
            .await
            .map_err(|p| p.detail)?;
        Ok(())
    }

    /// The workload as it is now.
    async fn read(&self, at: &Workload<'_>) -> Result<Value, String> {
        let object = self
            .cluster
            .get_object(at.kind, Some(at.namespace), at.name)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("the API is not served")?;
        let mut workload = serde_json::to_value(object).map_err(|e| e.to_string())?;
        if let Some(map) = workload.as_object_mut() {
            map.insert("apiVersion".to_owned(), json!("apps/v1"));
            map.insert("kind".to_owned(), json!(at.kind.kind));
        }
        Ok(workload)
    }

    /// Sends `operations` to the workload as one JSON Patch.
    async fn patch(&self, at: &Workload<'_>, operations: Vec<Value>) -> Result<(), String> {
        let request = WriteRequest {
            target: ObjectTarget {
                api_version: "apps/v1".to_owned(),
                kind: at.kind.kind.clone(),
                plural: at.kind.resource.clone(),
                namespace: at.namespace.to_owned(),
                name: at.name.to_owned(),
            },
            body: WriteBody::JsonPatch(Value::Array(operations)),
            dry_run: false,
        };
        self.cluster
            .write(&request)
            .await
            .map(|_written| ())
            .map_err(|e| e.to_string())
    }
}

/// One workload.
struct Workload<'a> {
    kind: &'a ResourceRef,
    namespace: &'a str,
    name: &'a str,
}

/// The fields of the rollout report the updater reads.
#[derive(Deserialize)]
struct Report {
    status: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    rollback: Option<Value>,
}

/// The time zone of an IANA name, from the database built into the binary.
fn zone(name: &str) -> Option<TimeZone> {
    TimeZone::get(name).ok()
}

/// The image of the workload's one container.
fn image_of(workload: &Value) -> Option<String> {
    match workload
        .pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([only]) => only.get("image").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;

    use serde_json::from_value;
    use zeabur_api::images::registry::{Answer, Get, Transport};
    use zeabur_kube::NamespaceObjects;
    use zeabur_kube::k8s_openapi::api::apps::v1::Deployment;
    use zeabur_kube::memory::{MemoryCluster, State as Memory};

    use super::*;

    #[test]
    fn the_zone_database_is_built_in() {
        // The image is `FROM scratch`: no /usr/share/zoneinfo to read.
        let amsterdam = zone("Europe/Amsterdam").unwrap();
        let winter: Timestamp = "2026-11-29T02:00:00Z".parse().unwrap();
        assert_eq!(
            winter.to_zoned(amsterdam).datetime().to_string(),
            "2026-11-29T03:00:00"
        );
        assert!(zone("Mars/Olympus").is_none());
    }

    /// Docker Hub's tag list of `library/redis`, without authentication.
    struct Hub;

    impl Transport for Hub {
        fn get<'a>(
            &'a self,
            get: Get,
        ) -> Pin<Box<dyn Future<Output = Result<Answer, String>> + Send + 'a>> {
            let answer =
                if get.url == "https://registry-1.docker.io/v2/library/redis/tags/list?n=1000" {
                    Answer {
                        status: 200,
                        body: serde_json::to_vec(
                            &json!({"tags": ["7.2.4", "7.2.5", "7.4.1", "8.0.0", "latest"]}),
                        )
                        .unwrap_or_default(),
                        ..Answer::default()
                    }
                } else {
                    Answer {
                        status: 404,
                        ..Answer::default()
                    }
                };
            Box::pin(async move { Ok(answer) })
        }
    }

    fn replica_set(revision: u32, image: &str, ready: i32) -> Value {
        json!({
            "metadata": {"name": format!("cache-{revision}abcd"), "namespace": "demo",
                "creationTimestamp": format!("2026-11-2{revision}T08:00:00Z"),
                "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment", "name": "cache",
                    "uid": "u1", "controller": true}],
                "annotations": {"deployment.kubernetes.io/revision": revision.to_string()}},
            "spec": {"selector": {"matchLabels": {"app": "cache"}},
                "template": {"metadata": {"labels": {"app": "cache"}},
                    "spec": {"containers": [{"name": "redis", "image": image}]}}},
            "status": {"replicas": 1, "readyReplicas": ready}
        })
    }

    fn pod(release: &str, waiting: Option<&str>) -> Value {
        let state = waiting.map_or_else(
            || json!({"running": {"startedAt": "2026-11-25T08:00:00Z"}}),
            |reason| json!({"waiting": {"reason": reason, "message": "back-off"}}),
        );
        json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": format!("{release}-x"), "namespace": "demo",
                "labels": {"app": "cache"},
                "ownerReferences": [{"apiVersion": "apps/v1", "kind": "ReplicaSet",
                    "name": release, "uid": "r", "controller": true}]},
            "spec": {"containers": [{"name": "redis", "image": "redis"}]},
            "status": {"phase": "Running", "containerStatuses": [{"name": "redis",
                "ready": waiting.is_none(), "restartCount": 5, "image": "redis", "imageID": "",
                "state": state, "lastState": {"terminated": {"exitCode": 1, "reason": "Error",
                    "finishedAt": "2026-11-25T08:05:00Z"}}}]}
        })
    }

    fn cache(cluster: &MemoryCluster) -> Value {
        cluster
            .inspect(|state| serde_json::to_value(&state.objects["demo"].deployments[0]))
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn a_patch_release_that_crash_loops_is_rolled_back() {
        let deployment = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "cache", "namespace": "demo", "uid": "u1", "generation": 3,
                "resourceVersion": "10",
                "labels": {"zeabur.com/image-updates": "patch"},
                "annotations": {"zeabur.com/image-update-window": "daily 00:00 24h UTC",
                    "deployment.kubernetes.io/revision": "1"}},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "cache"}},
                "template": {"metadata": {"labels": {"app": "cache"}},
                    "spec": {"containers": [{"name": "redis", "image": "redis:7.2.4"}]}}},
            "status": {"observedGeneration": 3, "replicas": 1, "updatedReplicas": 1,
                "readyReplicas": 1, "availableReplicas": 1, "conditions": [
                    {"type": "Available", "status": "True"},
                    {"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"}]}
        });
        let cluster = MemoryCluster::new(Memory {
            namespaces: vec![from_value(json!({"metadata": {"name": "demo"}})).unwrap()],
            objects: BTreeMap::from([(
                "demo".to_owned(),
                NamespaceObjects {
                    namespace: "demo".to_owned(),
                    deployments: vec![from_value(deployment).unwrap()],
                    ..NamespaceObjects::default()
                },
            )]),
            replica_sets: BTreeMap::from([(
                "demo".to_owned(),
                vec![from_value(replica_set(1, "redis:7.2.4", 1)).unwrap()],
            )]),
            ..Memory::default()
        });
        let shared: Arc<dyn Cluster> = Arc::new(cluster.clone());
        let updater = Updater {
            api: Api::new(Arc::clone(&shared), zeabur_api::Settings::default()),
            cluster: shared,
            registries: Inspector::new(Arc::new(Hub), Vec::new()),
        };

        // In the window: the newest 7.2 patch release, with 8.0.0 waiting for a person.
        updater.pass().await;
        let moved = cache(&cluster);
        assert_eq!(
            moved.pointer("/spec/template/spec/containers/0/image"),
            Some(&json!("redis:7.2.5"))
        );
        let record = record_of(&moved);
        assert_eq!(record.state, Some(State::Updating));
        assert_eq!(record.awaiting.as_deref(), Some("8.0.0"));

        // The controller rolls it out and the new pod crash loops.
        cluster.update(|state| {
            let objects = state.objects.get_mut("demo").unwrap();
            let deployment: &mut Deployment = &mut objects.deployments[0];
            let mut value = serde_json::to_value(&*deployment).unwrap();
            value["metadata"]["annotations"]["deployment.kubernetes.io/revision"] = json!("2");
            value["status"] = json!({"observedGeneration": 4, "replicas": 2, "updatedReplicas": 1,
                "readyReplicas": 1, "availableReplicas": 1});
            *deployment = from_value(value).unwrap();
            objects.pods = vec![
                from_value(pod("cache-1abcd", None)).unwrap(),
                from_value(pod("cache-2abcd", Some("CrashLoopBackOff"))).unwrap(),
            ];
            state
                .replica_sets
                .get_mut("demo")
                .unwrap()
                .push(from_value(replica_set(2, "redis:7.2.5", 0)).unwrap());
        });
        // Stuck: the failure is recorded, then the rollout report's rollback is applied.
        updater.pass().await;
        let back = cache(&cluster);
        assert_eq!(
            back.pointer("/spec/template/spec/containers/0/image"),
            Some(&json!("redis:7.2.4"))
        );
        // The rollback is recorded, and 7.2.5 is not tried again.
        updater.pass().await;
        let record = record_of(&cache(&cluster));
        assert_eq!(record.state, Some(State::RolledBack));
        assert_eq!(record.failed.as_deref(), Some("7.2.5"));
        assert_eq!(record.reason.as_deref(), Some("crashLoop"));
    }
}
