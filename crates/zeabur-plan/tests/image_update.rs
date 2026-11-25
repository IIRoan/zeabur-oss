//! An automatic image update's writes are made against the workload as read: they do not apply to
//! the workload after any other write, and the workload read again gives the answer for then.

#[cfg(test)]
mod tests {
    use k8s_openapi::jiff::Timestamp;
    use k8s_openapi::jiff::tz::TimeZone;
    use serde_json::{Value, json};
    use zeabur_plan::image_update::{
        Outcome, Rollout, State, follow, image_update, not_rolled_back, record_of,
    };
    use zeabur_plan::json::apply_patch;

    const UID: &str = "6f3d0c52-2a4e-4f7e-9d61-0c8a5b1e2f40";
    const NOW: &str = "2026-11-29T03:00:00Z";

    /// The parts of the workload that the writes below change.
    #[derive(Clone)]
    struct Read {
        uid: &'static str,
        resource_version: &'static str,
        generation: i64,
        labels: Value,
        annotations: Value,
        image: &'static str,
        available: i64,
    }

    impl Default for Read {
        fn default() -> Self {
            Self {
                uid: UID,
                resource_version: "41",
                generation: 3,
                labels: json!({"zeabur.com/image-updates": "patch"}),
                annotations: json!({"zeabur.com/image-update-window": "sun 03:00 2h UTC"}),
                image: "redis:7.2.4",
                available: 1,
            }
        }
    }

    fn deployment(read: &Read) -> Value {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "cache", "namespace": "demo", "uid": read.uid,
                "resourceVersion": read.resource_version, "generation": read.generation,
                "labels": read.labels, "annotations": read.annotations},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "cache"}}, "template": {
                "metadata": {"labels": {"app": "cache"}},
                "spec": {"containers": [{"name": "redis", "image": read.image}]}}},
            "status": {"observedGeneration": read.generation, "replicas": 1, "updatedReplicas": 1,
                "readyReplicas": read.available, "availableReplicas": read.available, "conditions": [
                    {"type": "Available", "status": "True"},
                    {"type": "Progressing", "status": "True", "reason": "NewReplicaSetAvailable"}]}
        })
    }

    fn at(now: &str) -> Timestamp {
        now.parse().unwrap()
    }

    fn utc(name: &str) -> Option<TimeZone> {
        (name == "UTC").then_some(TimeZone::UTC)
    }

    fn update(workload: &Value, now: &str) -> Result<Option<String>, String> {
        let tags = ["7.2.4", "7.2.5"].map(str::to_owned);
        image_update(workload, &tags, &utc, at(now))
            .map(|u| u.image)
            .map_err(|e| e.code)
    }

    fn write(workload: &Value) -> Vec<Value> {
        let tags = ["7.2.4", "7.2.5"].map(str::to_owned);
        image_update(workload, &tags, &utc, at(NOW))
            .unwrap()
            .operations
    }

    /// What was written, the workload after it, and what the rule answers when it is read again.
    type Since = (
        &'static str,
        Read,
        Result<Option<&'static str>, &'static str>,
    );

    /// The workload as kube-apiserver keeps it after another write, which raised its
    /// `resourceVersion`.
    fn written_since() -> Vec<Since> {
        let after = |read: Read| Read {
            resource_version: "42",
            ..read
        };
        vec![
            (
                "opted out",
                after(Read {
                    labels: json!({}),
                    ..Read::default()
                }),
                Err("not_opted_in"),
            ),
            (
                "Helm took it over (a label key that was absent)",
                after(Read {
                    labels: json!({"zeabur.com/image-updates": "patch",
                        "app.kubernetes.io/managed-by": "Helm"}),
                    ..Read::default()
                }),
                Err("managed_by_helm"),
            ),
            (
                "Argo CD tracks it (an annotation key that was absent)",
                after(Read {
                    annotations: json!({"zeabur.com/image-update-window": "sun 03:00 2h UTC",
                        "argocd.argoproj.io/tracking-id": "shop:apps/Deployment:demo/cache"}),
                    ..Read::default()
                }),
                Err("managed_by_argo_cd"),
            ),
            (
                "the window moved to another day",
                after(Read {
                    annotations: json!({"zeabur.com/image-update-window": "mon 03:00 2h UTC"}),
                    ..Read::default()
                }),
                Err("outside_window"),
            ),
            (
                "a person set the image by hand",
                after(Read {
                    image: "redis:7.2.4-alpine",
                    generation: 4,
                    ..Read::default()
                }),
                Ok(None),
            ),
            (
                "only its status was written",
                after(Read::default()),
                Ok(Some("redis:7.2.5")),
            ),
        ]
    }

    #[test]
    fn the_write_applies_to_the_workload_as_read() {
        let read = deployment(&Read::default());
        let written = apply_patch(&read, &write(&read)).unwrap();
        let record = record_of(&written);
        assert_eq!(record.state, Some(State::Updating));
        assert_eq!(record.checked, Some(at("2026-11-29T03:00:00Z")));
        assert_eq!(record.from.as_deref(), Some("redis:7.2.4"));
        assert_eq!(record.to.as_deref(), Some("redis:7.2.5"));
        let annotations = Read::default().annotations;
        let mut expected = Read {
            image: "redis:7.2.5",
            ..Read::default()
        };
        expected.annotations = json!({
            "zeabur.com/image-update-window": annotations["zeabur.com/image-update-window"],
            "zeabur.com/image-update-status": written["metadata"]["annotations"]["zeabur.com/image-update-status"],
        });
        assert_eq!(written, deployment(&expected));
    }

    #[test]
    fn no_write_since_the_read_is_overwritten() {
        let operations = write(&deployment(&Read::default()));
        for (what, stored, _) in written_since() {
            assert!(
                apply_patch(&deployment(&stored), &operations).is_err(),
                "{what}: the write still applied"
            );
        }
    }

    #[test]
    fn a_workload_made_again_under_the_same_name_does_not_take_the_write() {
        let operations = write(&deployment(&Read::default()));
        let again = Read {
            uid: "1c9e7a40-5b2d-4e8f-a3c6-7d0b9e1f2a53",
            ..Read::default()
        };
        assert!(apply_patch(&deployment(&again), &operations).is_err());
    }

    #[test]
    fn the_workload_read_again_gives_the_answer_for_then() {
        for (what, stored, expected) in written_since() {
            let answer = update(&deployment(&stored), NOW);
            let expected = expected
                .map(|i| i.map(str::to_owned))
                .map_err(str::to_owned);
            assert_eq!(answer, expected, "{what}");
        }
    }

    #[test]
    fn the_clock_read_again_after_a_conflict_can_close_the_window() {
        let stored = deployment(&Read {
            resource_version: "42",
            ..Read::default()
        });
        assert_eq!(
            update(&stored, "2026-11-29T05:00:00Z"),
            Err("outside_window".to_owned())
        );
    }

    #[test]
    fn an_overlapping_attempt_finds_the_first_one_followed() {
        let read = deployment(&Read::default());
        let first = apply_patch(&read, &write(&read)).unwrap();
        assert_eq!(update(&first, NOW), Err("updating".to_owned()));
    }

    /// The workload after the update was written and the controller took the new generation.
    fn after_update() -> Value {
        let read = deployment(&Read::default());
        let written = apply_patch(&read, &write(&read)).unwrap();
        deployment(&Read {
            resource_version: "57",
            generation: 4,
            image: "redis:7.2.5",
            annotations: written["metadata"]["annotations"].clone(),
            ..Read::default()
        })
    }

    fn stuck(workload: &Value) -> Vec<Value> {
        let stuck = Rollout::Stuck("crashLoop".to_owned());
        let followed = follow(workload, &stuck, at("2026-11-29T03:05:00Z")).unwrap();
        assert_eq!(followed.outcome, Outcome::RollBack);
        followed.operations
    }

    #[test]
    fn a_failed_rollout_is_recorded_before_and_after_its_rollback() {
        let stored = after_update();
        let before = apply_patch(&stored, &stuck(&stored)).unwrap();
        assert_eq!(record_of(&before).failed.as_deref(), Some("7.2.5"));
        let back = deployment(&Read {
            resource_version: "63",
            generation: 5,
            annotations: before["metadata"]["annotations"].clone(),
            ..Read::default()
        });
        let followed = follow(&back, &Rollout::Done, at("2026-11-29T03:06:00Z")).unwrap();
        assert_eq!(followed.outcome, Outcome::RolledBack);
        let recorded = apply_patch(&back, &followed.operations).unwrap();
        let record = record_of(&recorded);
        assert_eq!(record.state, Some(State::RolledBack));
        assert_eq!(record.reason.as_deref(), Some("crashLoop"));
        let next_week = recorded;
        let tags = ["7.2.4", "7.2.5"].map(str::to_owned);
        let checked = image_update(&next_week, &tags, &utc, at("2026-12-06T03:00:00Z")).unwrap();
        assert_eq!(checked.image, None);
    }

    #[test]
    fn a_record_is_not_written_over_a_change() {
        let stored = after_update();
        let operations = stuck(&stored);
        let mut again = stored.clone();
        again["metadata"]["uid"] = json!("1c9e7a40-5b2d-4e8f-a3c6-7d0b9e1f2a53");
        let mut recorded_elsewhere = stored;
        recorded_elsewhere["metadata"]["annotations"]["zeabur.com/image-update-status"] =
            json!("{\"state\":\"updated\"}");
        for (what, workload) in [
            ("made again", again),
            ("another updater recorded it", recorded_elsewhere),
        ] {
            assert!(
                apply_patch(&workload, &operations).is_err(),
                "{what}: the record still applied"
            );
        }
    }

    #[test]
    fn a_rollback_that_cannot_be_made_leaves_the_workload_to_a_person() {
        let stored = after_update();
        let before = apply_patch(&stored, &stuck(&stored)).unwrap();
        let operations = not_rolled_back(&before, "Deployment cache is managed by Helm").unwrap();
        let record = record_of(&apply_patch(&before, &operations).unwrap());
        assert_eq!(record.state, Some(State::Failed));
        assert_eq!(
            record.reason.as_deref(),
            Some("crashLoop; not rolled back: Deployment cache is managed by Helm")
        );
    }
}
