//! The rules automatic image updates follow.
//!
//! The image updater (`zeabur image-updates`) asks them about each opted-in workload it reads, and
//! sends the writes they give; how it runs is in `docs/internal/image-updates.md`.
//!
//! - **Opt-in** is the workload's label `zeabur.com/image-updates: patch`, and its window the
//!   annotation `zeabur.com/image-update-window`, such as `sun 03:00 2h Europe/Amsterdam`: the days
//!   (`daily` or `mon,…,sun`), the local start, how long it stays open, and an IANA time zone.
//! - **Only patch releases**: a tag `X.Y.Z` moves to the highest `X.Y.Z'` with `Z' > Z`, the same
//!   `v` prefix and the same suffix (`7.2.4-alpine` to `7.2.5-alpine`). The highest later minor or
//!   major release is recorded as waiting for a person. A tag of another form (`16.4`, `latest`)
//!   and an image pinned by digest do not move.
//! - **Nobody else moves it**: an object Helm, Argo CD or Flux manages, an image one of them owns,
//!   a workload a controller owns (an operator would restore its template) and one Keel updates
//!   (`keel.sh/policy`, which auto-deploy of builds sets) are refused.
//! - **Only a settled workload**: a Deployment or StatefulSet whose rollout kstatus counts as
//!   current, so an update never lands on a rollout in progress or failing. A StatefulSet with
//!   `OnDelete` or a partition is refused: not all of its pods take a new template on their own.
//! - **Only in the window**, with `now` the clock read just before the write, and once per
//!   occurrence of it: the write records the occurrence in `zeabur.com/image-update-status`.
//! - **The write** tests the workload's `uid` and `resourceVersion` as read, then the container and
//!   its image, and replaces the image. The `resourceVersion` is the server's version of the whole
//!   object [k8s-resource-version], so a write since the read, of the metadata or the status too,
//!   fails the patch.
//! - **Following the rollout** ([`follow`]) takes the service's rollout as the API reports it
//!   (`services/rollout`: done, still going, or stuck and why). A stuck rollout, or one still going
//!   when the deadline recorded with the update passes, is rolled back with the plan that report
//!   offers (`Change::Rollback`), once the tag is recorded as failed so it is not tried again. A
//!   workload whose spec, image or opt-in someone else changed meanwhile is no longer followed.
//!
//! [k8s-resource-version]: https://kubernetes.io/docs/reference/using-api/api-concepts/#resource-versions

use std::cmp::Ordering;
use std::str::FromStr;

use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{Container, PodTemplateSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::jiff::civil::{DateTime, Time};
use k8s_openapi::jiff::tz::TimeZone;
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeabur_model::adapters::controller_of;
use zeabur_model::adapters::provenance::detect_tool;
pub use zeabur_model::image_updates::{IMAGE_UPDATES, PATCH, Record, STATUS, State, WINDOW};
use zeabur_model::readiness::{self, Readiness, Reason};
use zeabur_model::{WorkloadKind, WorkloadRef};

use crate::builds::KEEL_POLICY;
use crate::fields::{Step, owners, reapplying_tool};
use crate::json::{add, pointer, remove, replace, test};
use crate::plan::{ErrorKind, PlanError, Planner, error, workload_json, workload_target};
use crate::project::{set_key, shown, steps};
use crate::rules::managed_code;

/// The shortest window.
const SHORTEST: SignedDuration = SignedDuration::from_mins(15);
/// The longest window: a day, so that two occurrences never overlap.
const LONGEST: SignedDuration = SignedDuration::from_hours(24);
/// How long a rollout may take when the workload sets no `progressDeadlineSeconds`, as
/// Kubernetes' default.
const PROGRESS_DEADLINE: i64 = 600;
/// How long past the progress deadline the updater waits for the controller to say so.
const GRACE: SignedDuration = SignedDuration::from_mins(1);

/// When a workload may be updated: on some days, from a local time, for a while, in a zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowSpec {
    /// Monday first.
    days: [bool; 7],
    start: Time,
    length: SignedDuration,
    zone: String,
}

/// Why a window cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidWindow;

impl std::fmt::Display for InvalidWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "A window is days (daily, or mon to sun separated by commas), a start (HH:MM), how \
             long it stays open (15m to 24h, such as 2h or 90m) and an IANA time zone.",
        )
    }
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Two digits.
fn two(part: &str) -> Option<i8> {
    (part.len() == 2 && part.bytes().all(|b| b.is_ascii_digit()))
        .then(|| part.parse().ok())
        .flatten()
}

impl FromStr for WindowSpec {
    type Err = InvalidWindow;

    fn from_str(text: &str) -> Result<Self, InvalidWindow> {
        let mut parts = text.split_whitespace();
        let (Some(days), Some(start), Some(length), Some(zone), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(InvalidWindow);
        };
        let chosen = if days == "daily" {
            [true; 7]
        } else {
            let mut chosen = [false; 7];
            for day in days.split(',') {
                let at = DAYS.iter().position(|d| *d == day).ok_or(InvalidWindow)?;
                *chosen.get_mut(at).ok_or(InvalidWindow)? = true;
            }
            chosen
        };
        let (hour, minute) = start.split_once(':').ok_or(InvalidWindow)?;
        let start = Time::new(
            two(hour).ok_or(InvalidWindow)?,
            two(minute).ok_or(InvalidWindow)?,
            0,
            0,
        )
        .map_err(|_range| InvalidWindow)?;
        let length: SignedDuration = length.parse().map_err(|_syntax| InvalidWindow)?;
        let zone_valid = zone
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'));
        if !(SHORTEST..=LONGEST).contains(&length) || !zone_valid {
            return Err(InvalidWindow);
        }
        Ok(Self {
            days: chosen,
            start,
            length,
            zone: zone.to_owned(),
        })
    }
}

impl WindowSpec {
    /// The IANA time zone it is in.
    #[must_use]
    pub fn zone(&self) -> &str {
        &self.zone
    }

    /// The occurrence open at `now` in `tz`, if one is. A start that a change to daylight saving
    /// time skips is taken as the same time after the change, and one it repeats as the first.
    #[must_use]
    pub fn occurrence(&self, tz: &TimeZone, now: Timestamp) -> Option<Window> {
        let mut date = now.to_zoned(tz.clone()).date();
        // A window opened yesterday can still be open; a day is the longest.
        for _ in 0..2 {
            let chosen = usize::try_from(date.weekday().to_monday_zero_offset())
                .ok()
                .and_then(|day| self.days.get(day).copied())
                .unwrap_or(false);
            if chosen {
                let start = DateTime::from_parts(date, self.start)
                    .to_zoned(tz.clone())
                    .ok()?
                    .timestamp();
                let end = start.checked_add(self.length).ok()?;
                if start <= now && now < end {
                    return Some(Window { start, end });
                }
            }
            date = date.yesterday().ok()?;
        }
        None
    }
}

/// One occurrence of a maintenance window, from `start` up to but not including `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// When it opens.
    pub start: Timestamp,
    /// When it closes.
    pub end: Timestamp,
}

/// The record a workload, as read, carries; the default when it has none.
#[must_use]
pub fn record_of(workload: &Value) -> Record {
    let annotations = workload
        .pointer("/metadata/annotations")
        .cloned()
        .and_then(|a| serde_json::from_value(a).ok());
    Record::from_annotations(annotations.as_ref())
}

/// What the rule allows for one workload in its window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageUpdate {
    /// The container whose image it moves.
    pub container: String,
    /// The image after the write; absent when no newer patch release is listed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The write, RFC 6902: the new image, if any, and the record of this check.
    pub operations: Vec<Value>,
    /// The highest listed tag of a later minor or major release, which waits for a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub awaiting: Option<String>,
}

/// A tag `[v]X.Y.Z[-suffix]`: a semantic version [semver] after an optional `v`, its pre-release
/// part taken as the suffix that names the image's variant.
///
/// [semver]: https://semver.org/spec/v2.0.0.html
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version<'a> {
    prefix: &'a str,
    version: semver::Version,
}

impl<'a> Version<'a> {
    fn parse(tag: &'a str) -> Option<Self> {
        let (prefix, rest) = tag.strip_prefix('v').map_or(("", tag), |rest| ("v", rest));
        let version = semver::Version::parse(rest)
            .ok()
            .filter(|v| v.build.is_empty())?;
        Some(Self { prefix, version })
    }

    fn same_line(&self, other: &Self) -> bool {
        self.prefix == other.prefix && self.version.pre == other.version.pre
    }

    const fn order(&self) -> (u64, u64, u64) {
        (self.version.major, self.version.minor, self.version.patch)
    }
}

/// The repository and tag of an image without a digest; `None` when it names no tag.
#[must_use]
pub fn split_tag(image: &str) -> Option<(&str, &str)> {
    let (repository, tag) = image.rsplit_once(':')?;
    (!tag.contains('/')).then_some((repository, tag))
}

/// What a workload is, read once for the rules.
struct Read {
    kind: &'static str,
    name: String,
    meta: ObjectMeta,
    readiness: Readiness,
    containers: Vec<Container>,
    progress_deadline: Option<i32>,
}

/// Whether `workload`, read just now, may move to a newer patch release of its image at `now`.
///
/// Given the tags its repository lists, the write that moves it and records the check. `zone`
/// gives the time zone of an IANA name.
///
/// # Errors
///
/// A refusal with a stable code: `not_opted_in`, `no_window`, `invalid_window`,
/// `unknown_time_zone`, `outside_window`, `updating`, `already_checked`, `not_updatable`,
/// `managed_by_helm`, `managed_by_argo_cd`, `managed_by_flux`, `owned_by_controller`,
/// `updated_by_keel`, `container_required`, `field_managed_by_tool`, `pinned_by_digest`,
/// `no_version_tag`, `rollout_not_followed`, `not_ready`, `not_pinned`.
pub fn image_update(
    workload: &Value,
    tags: &[String],
    zone: &dyn Fn(&str) -> Option<TimeZone>,
    now: Timestamp,
) -> Result<ImageUpdate, PlanError> {
    let read = read(workload)?;
    let window = window(&read, zone, now)?;
    let record = record_of(workload);
    if record.state == Some(State::Updating) {
        return Err(error(
            ErrorKind::Conflict,
            "updating",
            "The last update's rollout is still followed.",
        ));
    }
    if record.checked == Some(window.start) {
        return Err(error(
            ErrorKind::Conflict,
            "already_checked",
            format!(
                "This window, open since {}, was checked already.",
                window.start
            ),
        ));
    }
    refuse_other_writers(&read.meta, read.kind, &read.name)?;
    let (container, repository, current) = updatable_image(workload, &read)?;
    let image = container.image.as_deref().unwrap_or_default();
    settled(read.readiness)?;
    let (uid, version) = pins(&read.meta)?;
    let (patch, awaiting) = newer(&current, tags, record.failed.as_deref());
    let mut next = Record {
        checked: Some(window.start),
        awaiting: awaiting.map(str::to_owned),
        ..record
    };
    let mut operations = vec![
        test("/metadata/uid", json!(uid)),
        test("/metadata/resourceVersion", json!(version)),
    ];
    let image_after = patch.map(|tag| format!("{repository}:{tag}"));
    if let Some(after) = &image_after {
        let at = "/spec/template/spec/containers/0";
        operations.extend([
            test(&format!("{at}/name"), json!(container.name)),
            test(&format!("{at}/image"), json!(image)),
            replace(&format!("{at}/image"), json!(after)),
        ]);
        let wait = SignedDuration::from_secs(
            i64::from(read.progress_deadline.unwrap_or_default()).max(PROGRESS_DEADLINE),
        );
        next = Record {
            state: Some(State::Updating),
            from: Some(image.to_owned()),
            to: Some(after.clone()),
            at: Some(now),
            generation: read.meta.generation.map(|g| g.saturating_add(1)),
            deadline: wait
                .checked_add(GRACE)
                .and_then(|wait| now.checked_add(wait).ok()),
            reason: None,
            ..next
        };
    }
    operations.push(add(
        &pointer(&["metadata", "annotations", STATUS]),
        json!(next.to_annotation()),
    ));
    Ok(ImageUpdate {
        container: container.name.clone(),
        image: image_after,
        operations,
        awaiting: next.awaiting,
    })
}

/// The service's rollout, as `GET …/services/{kind}/{name}/rollout` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rollout {
    /// `done`: every pod runs the current release and is available.
    Done,
    /// `progressing`, `paused` or `waitsForDeletion`: it may still finish.
    Progressing,
    /// `stuck`, with the report's reason: it will not finish by itself.
    Stuck(String),
}

/// The workload's one container, its image's repository and the version its tag names, refusing
/// an image a tool owns, one pinned by digest and a tag that is not a version.
fn updatable_image<'r>(
    workload: &Value,
    read: &'r Read,
) -> Result<(&'r Container, &'r str, Version<'r>), PlanError> {
    let [container] = read.containers.as_slice() else {
        return Err(error(
            ErrorKind::BadRequest,
            "container_required",
            "Only a workload with one container is updated.",
        ));
    };
    let image_steps = [
        Step::field("spec"),
        Step::field("template"),
        Step::field("spec"),
        Step::field("containers"),
        Step::named(&container.name),
        Step::field("image"),
    ];
    if let Some(manager) = owners(workload, &image_steps)
        .into_iter()
        .find(|m| reapplying_tool(m).is_some())
    {
        return Err(error(
            ErrorKind::Conflict,
            "field_managed_by_tool",
            format!(
                "The image of container {} is managed by {manager}, which would restore it.",
                container.name
            ),
        ));
    }
    let image = container.image.as_deref().unwrap_or_default();
    if image.contains('@') {
        return Err(error(
            ErrorKind::Invalid,
            "pinned_by_digest",
            format!("{image} is pinned by digest."),
        ));
    }
    split_tag(image)
        .and_then(|(repository, tag)| Version::parse(tag).map(|v| (container, repository, v)))
        .ok_or_else(|| {
            error(
                ErrorKind::Invalid,
                "no_version_tag",
                format!("{image} has no tag of the form X.Y.Z."),
            )
        })
}

/// Where a followed rollout is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// Still rolling out; nothing to write.
    Waiting,
    /// Done: so is the update.
    Updated,
    /// Failed: after the write, which records the tag as failed, roll back with the rollout
    /// report's plan.
    RollBack,
    /// The release before is back since the update; the write records it.
    RolledBack,
    /// Someone else changed the workload since the update; no longer followed.
    Superseded,
}

/// The rollout of the last update, and the write that records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Followed {
    /// Where it is.
    pub outcome: Outcome,
    /// The write, RFC 6902; empty when there is nothing to record.
    pub operations: Vec<Value>,
}

/// Where the rollout of the update recorded on `workload`, read just now, is, given the
/// service's `rollout` and the time.
///
/// # Errors
///
/// `not_updating` when no update is followed, `not_updatable` for another kind, `not_pinned`
/// without a `uid`.
pub fn follow(workload: &Value, rollout: &Rollout, now: Timestamp) -> Result<Followed, PlanError> {
    let read = read(workload)?;
    let record = record_of(workload);
    let (Some(to), Some(from), Some(generation)) = (
        record.to.as_deref(),
        record.from.as_deref(),
        record.generation,
    ) else {
        return Err(not_updating());
    };
    if record.state != Some(State::Updating) {
        return Err(not_updating());
    }
    let opted_in = read
        .meta
        .labels
        .as_ref()
        .and_then(|l| l.get(IMAGE_UPDATES))
        .is_some_and(|v| v == PATCH);
    let image = match read.containers.as_slice() {
        [only] => only.image.as_deref(),
        _ => None,
    };
    let changed = read.meta.generation != Some(generation);
    let failed = split_tag(to)
        .map(|(_, tag)| tag.to_owned())
        .or_else(|| record.failed.clone());
    let (outcome, next) = if changed && image == Some(from) {
        let next = Record {
            state: Some(State::RolledBack),
            failed,
            reason: record
                .reason
                .clone()
                .or_else(|| Some("rolled back".to_owned())),
            ..record.clone()
        };
        (Outcome::RolledBack, Some(next))
    } else if changed || image != Some(to) || !opted_in {
        let next = Record {
            state: Some(State::Superseded),
            ..record.clone()
        };
        (Outcome::Superseded, Some(next))
    } else {
        let reason = match rollout {
            Rollout::Done => {
                let next = Record {
                    state: Some(State::Updated),
                    ..record.clone()
                };
                return recorded(workload, &read, Outcome::Updated, Some(&next));
            }
            Rollout::Stuck(reason) => reason.clone(),
            Rollout::Progressing if record.deadline.is_some_and(|d| now >= d) => {
                "not done by the deadline".to_owned()
            }
            Rollout::Progressing => {
                return recorded(workload, &read, Outcome::Waiting, None);
            }
        };
        // Recorded before the rollback is sent, once: a crash in between keeps the tag failed.
        let next = record.reason.is_none().then(|| Record {
            failed,
            reason: Some(reason),
            ..record.clone()
        });
        (Outcome::RollBack, next)
    };
    recorded(workload, &read, outcome, next.as_ref())
}

fn not_updating() -> PlanError {
    error(
        ErrorKind::Conflict,
        "not_updating",
        "No update's rollout is followed.",
    )
}

/// `outcome`, with the write of `next` that tests the `uid` and the record as read.
fn recorded(
    workload: &Value,
    read: &Read,
    outcome: Outcome,
    next: Option<&Record>,
) -> Result<Followed, PlanError> {
    let Some(next) = next else {
        return Ok(Followed {
            outcome,
            operations: Vec::new(),
        });
    };
    let Some(uid) = read.meta.uid.as_deref().filter(|u| !u.is_empty()) else {
        return Err(error(
            ErrorKind::Invalid,
            "not_pinned",
            "The workload as read has no uid.",
        ));
    };
    let status_path = pointer(&["metadata", "annotations", STATUS]);
    let status_now = workload
        .pointer(&status_path)
        .cloned()
        .unwrap_or(Value::Null);
    Ok(Followed {
        outcome,
        operations: vec![
            test("/metadata/uid", json!(uid)),
            test(&status_path, status_now),
            add(&status_path, json!(next.to_annotation())),
        ],
    })
}

/// The write that records a rollback that could not be made: the workload is left to a person.
///
/// # Errors
///
/// `not_updating` when no update is followed, `not_pinned` without a `uid`.
pub fn not_rolled_back(workload: &Value, why: &str) -> Result<Vec<Value>, PlanError> {
    let read = read(workload)?;
    let record = record_of(workload);
    if record.state != Some(State::Updating) {
        return Err(not_updating());
    }
    let next = Record {
        state: Some(State::Failed),
        reason: Some(record.reason.as_ref().map_or_else(
            || format!("not rolled back: {why}"),
            |reason| format!("{reason}; not rolled back: {why}"),
        )),
        ..record
    };
    recorded(workload, &read, Outcome::RollBack, Some(&next)).map(|f| f.operations)
}

/// The workload's kind, name, metadata, readiness and containers.
fn read(workload: &Value) -> Result<Read, PlanError> {
    let typed = |e: serde_json::Error| error(ErrorKind::Invalid, "not_updatable", e.to_string());
    let containers = |spec: Option<PodTemplateSpec>| {
        spec.and_then(|t| t.spec)
            .map_or_else(Vec::new, |s| s.containers)
    };
    let read = match workload.get("kind").and_then(Value::as_str) {
        Some("Deployment") => {
            let deployment: Deployment = serde_json::from_value(workload.clone()).map_err(typed)?;
            let readiness = readiness::deployment(&deployment);
            let progress_deadline = deployment
                .spec
                .as_ref()
                .and_then(|s| s.progress_deadline_seconds);
            Read {
                kind: "Deployment",
                name: deployment.metadata.name.clone().unwrap_or_default(),
                containers: containers(deployment.spec.map(|s| s.template)),
                meta: deployment.metadata,
                readiness,
                progress_deadline,
            }
        }
        Some("StatefulSet") => {
            let set: StatefulSet = serde_json::from_value(workload.clone()).map_err(typed)?;
            let readiness = readiness::stateful_set(&set);
            Read {
                kind: "StatefulSet",
                name: set.metadata.name.clone().unwrap_or_default(),
                containers: containers(set.spec.map(|s| s.template)),
                meta: set.metadata,
                readiness,
                progress_deadline: None,
            }
        }
        kind => {
            return Err(error(
                ErrorKind::Invalid,
                "not_updatable",
                format!(
                    "{} is not a Deployment or StatefulSet.",
                    kind.unwrap_or("The object")
                ),
            ));
        }
    };
    Ok(read)
}

/// The occurrence of the workload's window open at `now`, refusing one that is not opted in.
fn window(
    read: &Read,
    zone: &dyn Fn(&str) -> Option<TimeZone>,
    now: Timestamp,
) -> Result<Window, PlanError> {
    let (kind, name) = (read.kind, &read.name);
    if read
        .meta
        .labels
        .as_ref()
        .and_then(|l| l.get(IMAGE_UPDATES))
        .map(String::as_str)
        != Some(PATCH)
    {
        return Err(error(
            ErrorKind::Conflict,
            "not_opted_in",
            format!("{kind} {name} has no {IMAGE_UPDATES}: {PATCH} label."),
        ));
    }
    let Some(text) = read.meta.annotations.as_ref().and_then(|a| a.get(WINDOW)) else {
        return Err(error(
            ErrorKind::Conflict,
            "no_window",
            format!("{kind} {name} has no {WINDOW} annotation."),
        ));
    };
    let spec: WindowSpec = text
        .parse()
        .map_err(|e: InvalidWindow| error(ErrorKind::Invalid, "invalid_window", e.to_string()))?;
    let tz = zone(spec.zone()).ok_or_else(|| {
        error(
            ErrorKind::Invalid,
            "unknown_time_zone",
            format!("{} is not a known time zone.", spec.zone()),
        )
    })?;
    spec.occurrence(&tz, now).ok_or_else(|| {
        error(
            ErrorKind::Conflict,
            "outside_window",
            format!("It is {now}, outside the window {text}."),
        )
    })
}

/// Refuses a workload that Helm, Argo CD, Flux, a controller or Keel writes as well.
fn refuse_other_writers(meta: &ObjectMeta, kind: &str, name: &str) -> Result<(), PlanError> {
    if let Some(tool) = detect_tool(meta) {
        return Err(PlanError {
            managed_by: Some(tool),
            ..error(
                ErrorKind::Conflict,
                managed_code(tool),
                format!("{kind} {name} is managed by a tool that would restore its image."),
            )
        });
    }
    if let Some(owner) = controller_of(meta) {
        return Err(error(
            ErrorKind::Conflict,
            "owned_by_controller",
            format!(
                "{kind} {name} belongs to {} {}, which would restore its image.",
                owner.kind, owner.name
            ),
        ));
    }
    if meta
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(KEEL_POLICY))
    {
        return Err(error(
            ErrorKind::Conflict,
            "updated_by_keel",
            format!("{kind} {name} is updated by Keel ({KEEL_POLICY})."),
        ));
    }
    Ok(())
}

/// Refuses a rollout that is not current, or one whose pods do not all move on their own.
fn settled(readiness: Readiness) -> Result<(), PlanError> {
    if matches!(
        readiness.reason,
        Reason::OnDelete | Reason::PartitionRolledOut | Reason::PartitionRollout
    ) {
        return Err(error(
            ErrorKind::Invalid,
            "rollout_not_followed",
            "The StatefulSet updates with OnDelete or a partition, so not every pod moves on its \
             own.",
        ));
    }
    if readiness.is_current() {
        Ok(())
    } else {
        Err(error(
            ErrorKind::Conflict,
            "not_ready",
            format!("The rollout is not current ({}).", readiness.reason),
        ))
    }
}

/// The `uid` and `resourceVersion` the write is made against.
fn pins(meta: &ObjectMeta) -> Result<(&str, &str), PlanError> {
    match (meta.uid.as_deref(), meta.resource_version.as_deref()) {
        (Some(uid), Some(version)) if !uid.is_empty() && !version.is_empty() => Ok((uid, version)),
        _ => Err(error(
            ErrorKind::Invalid,
            "not_pinned",
            "The workload as read has no uid or resourceVersion.",
        )),
    }
}

/// The highest newer patch release of `current` other than `failed`, and the highest later minor
/// or major release.
fn newer<'t>(
    current: &Version<'_>,
    tags: &'t [String],
    failed: Option<&str>,
) -> (Option<&'t str>, Option<&'t str>) {
    let mut patch: Option<((u64, u64, u64), &'t str)> = None;
    let mut later: Option<((u64, u64, u64), &'t str)> = None;
    let (major, minor, now) = current.order();
    for tag in tags {
        let Some(order) = Version::parse(tag)
            .filter(|v| v.same_line(current))
            .map(|v| v.order())
        else {
            continue;
        };
        let slot = match (order.0, order.1).cmp(&(major, minor)) {
            Ordering::Equal if order.2 > now && failed != Some(tag) => &mut patch,
            Ordering::Greater => &mut later,
            Ordering::Less | Ordering::Equal => continue,
        };
        if slot.is_none_or(|(best, _)| order > best) {
            *slot = Some((order, tag.as_str()));
        }
    }
    (patch.map(|(_, t)| t), later.map(|(_, t)| t))
}

#[expect(
    clippy::multiple_inherent_impl,
    reason = "the planner's change kinds are split across modules by area"
)]
impl Planner<'_> {
    /// Opts a workload in to image updates with `window`, moves its window, or (`None`) opts it
    /// out. The record of earlier updates stays.
    pub(crate) fn image_updates(
        &mut self,
        reference: &WorkloadRef,
        window: Option<&str>,
    ) -> Result<(), PlanError> {
        let found = self.workload(reference)?;
        if !matches!(
            found.kind(),
            WorkloadKind::Deployment | WorkloadKind::StatefulSet
        ) {
            return Err(error(
                ErrorKind::Invalid,
                "not_updatable",
                format!(
                    "{} {} is not a Deployment or StatefulSet.",
                    reference.kind.as_str(),
                    reference.name
                ),
            ));
        }
        let (kind, name) = (reference.kind.as_str(), reference.name.as_str());
        Self::refuse_managed(found.meta(), kind, name)?;
        refuse_other_writers(found.meta(), kind, name)?;
        let index = self.working(workload_target(found), workload_json(found)?);
        let current = self.current(index);
        let label = pointer(&["metadata", "labels", IMAGE_UPDATES]);
        let annotation = pointer(&["metadata", "annotations", WINDOW]);
        let (label_now, window_now) = (
            current.pointer(&label).cloned(),
            current.pointer(&annotation).cloned(),
        );
        let window = window.map(str::trim).filter(|w| !w.is_empty());
        if let Some(window) = window {
            window
                .parse::<WindowSpec>()
                .map_err(|e| error(ErrorKind::BadRequest, "invalid_window", e.to_string()))?;
            updatable_image(&current, &read(&current)?)?;
        }
        let wanted = window.map(|w| (json!(PATCH), json!(w)));
        if wanted.as_ref().map(|(l, w)| (Some(l), Some(w)))
            == Some((label_now.as_ref(), window_now.as_ref()))
            || (wanted.is_none() && label_now.is_none() && window_now.is_none())
        {
            return Err(error(
                ErrorKind::BadRequest,
                "unchanged",
                format!(
                    "{kind} {name} is already {}.",
                    if wanted.is_some() {
                        "updated in this window"
                    } else {
                        "updated by hand"
                    }
                ),
            ));
        }
        for (map, key, path, before, after) in [
            (
                "labels",
                IMAGE_UPDATES,
                &label,
                label_now,
                wanted.as_ref().map(|(l, _)| l),
            ),
            (
                "annotations",
                WINDOW,
                &annotation,
                window_now,
                wanted.as_ref().map(|(_, w)| w),
            ),
        ] {
            let operations = match (before, after) {
                (Some(before), Some(after)) if &before == after => continue,
                (Some(before), Some(after)) => {
                    vec![test(path, before), replace(path, after.clone())]
                }
                (None, Some(after)) => set_key(
                    &self.current(index),
                    map,
                    key,
                    after.as_str().unwrap_or_default(),
                ),
                (Some(before), None) => vec![test(path, before), remove(path)],
                (None, None) => continue,
            };
            self.claim(index, &steps(map, key), &shown(map, key))?;
            self.patch(index, operations)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_a_version_only_as_x_y_z() {
        let parsed = |tag| Version::parse(tag).map(|v| v.order());
        assert_eq!(parsed("7.2.4"), Some((7, 2, 4)));
        assert_eq!(parsed("v1.0.10-alpine3.20"), Some((1, 0, 10)));
        for tag in [
            "16.4", "7", "latest", "7.2.4.1", "07.2.4", "7..4", "7.2.x", "",
        ] {
            assert_eq!(parsed(tag), None, "{tag}");
        }
    }

    #[test]
    fn a_registry_port_is_not_a_tag() {
        assert_eq!(split_tag("registry:5000/redis"), None);
        assert_eq!(
            split_tag("registry:5000/redis:7.2.4"),
            Some(("registry:5000/redis", "7.2.4"))
        );
    }

    #[test]
    fn a_window_is_days_a_start_a_length_and_a_zone() {
        for good in [
            "sun 03:00 2h Europe/Amsterdam",
            "mon,thu 23:30 90m UTC",
            "daily 00:00 24h America/Argentina/Buenos_Aires",
        ] {
            assert!(good.parse::<WindowSpec>().is_ok(), "{good}");
        }
        for bad in [
            "",
            "sun 03:00 2h",
            "sunday 03:00 2h UTC",
            "sun 3:00 2h UTC",
            "sun 24:00 2h UTC",
            "sun 03:00 10m UTC",
            "sun 03:00 25h UTC",
            "sun 03:00 2h Europe/Ams terdam",
            "sun,, 03:00 2h UTC",
        ] {
            assert_eq!(bad.parse::<WindowSpec>(), Err(InvalidWindow), "{bad:?}");
        }
    }
}
