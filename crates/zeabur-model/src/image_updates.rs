//! Automatic image updates as a workload carries them: the label that opts it in, the annotation
//! that names its window, and the record the image updater keeps (`zeabur_plan::image_update`).

use std::collections::BTreeMap;

use k8s_openapi::jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// The workload label that opts it in, with the policy as its value.
pub const IMAGE_UPDATES: &str = "zeabur.com/image-updates";
/// The one policy: patch releases only.
pub const PATCH: &str = "patch";
/// The workload annotation that names its window.
pub const WINDOW: &str = "zeabur.com/image-update-window";
/// The workload annotation the updater keeps its [`Record`] in.
pub const STATUS: &str = "zeabur.com/image-update-status";

/// Where the last update is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema), schema(as = ImageUpdateState))]
#[serde(rename_all = "camelCase")]
pub enum State {
    /// Written; its rollout is followed.
    Updating,
    /// Its rollout finished.
    Updated,
    /// Its rollout failed and the release before is back.
    RolledBack,
    /// Its rollout failed and could not be rolled back: the workload is left to a person.
    Failed,
    /// Someone else changed the workload during its rollout, which is no longer followed.
    Superseded,
}

/// What the updater last did to a workload, kept in its [`STATUS`] annotation as compact JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema), schema(as = ImageUpdateRecord))]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// The state of the last update; absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    /// The start of the window occurrence last checked, at most once each.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<Timestamp>,
    /// The image before the last update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The image the last update set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// When the last update was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<Timestamp>,
    /// The workload's `metadata.generation` after that write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
    /// When the rollout counts as failed if it has not finished by then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<Timestamp>,
    /// A tag whose rollout failed, not tried again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
    /// The highest later minor or major release, which waits for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<String>,
    /// Why the last update failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Record {
    /// The record in a workload's `annotations`; the default when there is none, or one that
    /// cannot be read.
    #[must_use]
    pub fn from_annotations(annotations: Option<&BTreeMap<String, String>>) -> Self {
        annotations
            .and_then(|a| a.get(STATUS))
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or_default()
    }

    /// The annotation's value.
    #[must_use]
    pub fn to_annotation(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// A workload's automatic image updates, as its metadata says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "camelCase")]
pub struct ImageUpdates {
    /// The window, as the annotation names it, such as `sun 03:00 2h Europe/Amsterdam`; empty
    /// when it names none.
    pub window: String,
    /// What the image updater last did.
    pub last: Record,
}

impl ImageUpdates {
    /// The image updates of a workload with these `labels` and `annotations`; `None` unless it is
    /// opted in.
    #[must_use]
    pub fn from_metadata(
        labels: Option<&BTreeMap<String, String>>,
        annotations: Option<&BTreeMap<String, String>>,
    ) -> Option<Self> {
        labels
            .and_then(|l| l.get(IMAGE_UPDATES))
            .filter(|v| *v == PATCH)?;
        Some(Self {
            window: annotations
                .and_then(|a| a.get(WINDOW))
                .cloned()
                .unwrap_or_default(),
            last: Record::from_annotations(annotations),
        })
    }
}
