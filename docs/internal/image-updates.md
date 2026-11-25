# Automatic image updates

Issue [#1004](https://github.com/zeabur-oss/zeabur/issues/1004): opt-in automatic image updates per
service, patch releases only (Redis 7.2.4 to 7.2.5; minor and major versions wait for a person), in
a window in a named time zone, done only once the service is healthy again, rolled back when it is
not, and a message when something goes wrong.

```text
person ──plan (Change::ImageUpdates)──▶ label zeabur.com/image-updates: patch
                                        annotation zeabur.com/image-update-window
zeabur image-updates (own ServiceAccount, chart imageUpdates.enabled)
  every interval: list opted-in Deployments and StatefulSets (one list per kind, by label)
    in the window, once per occurrence:  registry tags ─▶ zeabur_plan::image_update ─▶ JSON Patch
    while an update rolls out:           GET …/rollout ─▶ zeabur_plan::image_update::follow
      stuck or late:                     the report's Change::Rollback, through plan and apply
  record: annotation zeabur.com/image-update-status ─▶ the service's Settings
failed rollout ─▶ the project's notification channels (crash-loop, deploy-failed), as any other
```

## Keel cannot keep a window

Zeabur OSS already hands Git-built workloads to Keel (`keel.sh/policy: force`), and the catalog
installs Keel 0.22.4 (chart v1.2.3, `fixtures/charts/keel.yaml`). Read at that tag:

- The first poll of a new watch runs at once (`trigger/poll/watcher.go` `addJob`), and watches live
  in memory, so every restart of Keel polls at whatever time that is.
- One watch per image serves the whole cluster: the last `keel.sh/pollSchedule` seen wins, and when
  it fires `multi_tags_watcher.go` updates every workload of that image, whatever schedule each set.
- Its cron (`rusenask/cron` v1.1.0) takes no time zone and runs in the process's, UTC.
- "Successfully updated" means kube-apiserver took the write (`applyPlan`); Keel does not follow
  the rollout or roll back.

So `keel.sh/pollSchedule` says how often Keel looks, not when a workload may change. Builds keep
using Keel; the image update rules refuse a workload with `keel.sh/policy`, so the two never write
the same workload.

## Who writes

Zeabur OSS's server acts only as the person, and member roles have no RBAC writes, so neither the
server nor a person can give each service an updater of its own. The updater is
`zeabur image-updates`, a subcommand of the same binary in a Deployment the chart adds when an
admin sets `imageUpdates.enabled`, acting as its own ServiceAccount (`UserClient::with_token_file`),
as `zeabur components install` acts as its Job's. That is a deliberate exception to "never a client
from a service account" (`docs/internal/crates.md`): the admin turns it on, and its rights are

- the project viewer's and a member's reads (the chart's `zeabur-project-viewer` and
  `zeabur-member` roles), which the API needs to report a rollout and plan a rollback;
- `patch` of Deployments and StatefulSets, which the ValidatingAdmissionPolicy
  `zeabur-image-updater` holds to workloads opted in before and after the write, with the same
  replicas, pod-level settings, volumes, and each container's repository, command, environment,
  mounts and security context. Admission has no clock (Kubernetes 1.34's CEL environment,
  `apiserver/pkg/cel/environment/base.go`), so the window is the updater's to keep.

It calls the API in its own process (`crates/zeabur/src/api.rs`, shared with
`zeabur components install`), so a rollback is the same plan, preconditions and dry run as a
person's.

## The rules (`zeabur_plan::image_update`)

- **Opt-in**: the label, set with the window by a person's plan. The planner refuses what the
  updater would refuse anyway (Helm, Argo CD, Flux, an operator, Keel, a tag that is not `X.Y.Z`),
  and the API refuses a time zone the built-in database does not know.
- **Window**: `<days> <HH:MM> <15m..24h> <IANA zone>`, evaluated with jiff and its time zone
  database built into the binary (`tzdb-bundle-always`: the image is `FROM scratch`). A start that
  daylight saving time skips is taken after the change, one it repeats at the first. The rule is
  asked with the clock read just before the write.
- **Once per occurrence**: the write records the occurrence's start (`checked`) with the new image
  in the same patch, so a restart, a second updater or an overlapping pass does not try again.
- **Patch releases**: the tag is a semantic version after an optional `v`; its pre-release part
  is the suffix that names the variant (`-alpine`) and must match. The highest later minor or
  major release is recorded as `awaiting`. PostgreSQL's two-part `16.4` does not move.
- **The write**: `test` of `uid` and `resourceVersion` as read, then the container and image, and
  the record. A `resourceVersion` conflict means the workload changed, metadata and status
  included; the next pass reads it and the clock again.

## Health and rollback

The updater does not judge health itself. It reads the service's rollout as the interface does
(`GET …/services/{kind}/{name}/rollout`, `services/rollout.rs`): done, still going, or stuck with a
reason (a crash loop, an image that cannot be pulled, out of memory, `ProgressDeadlineExceeded`, a
quota, …). Done records the update. Stuck, or still going at the deadline recorded with the update
(the progress deadline, at least 600 s, plus a minute), first records the tag as failed and then
sends the report's rollback (`Change::Rollback`) through plan and apply. The next pass sees the
release before back and records it. A rollback the plan refuses records the failure and leaves the
workload to a person. Someone who changes the spec, image or opt-in during the rollout takes it
over: the update is recorded as superseded and nothing is rolled back.

A StatefulSet with `OrderedReady` keeps a failed pod after its template is restored
([forced rollback](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/#forced-rollback));
the rollback is applied, and that pod still waits for a person to delete it.

## Notifications

Zeabur OSS's project notification channels (`zeabur_modules::alertmanager`) already route a failed
rollout: the catalog's `zeabur-notifications` rules fire `crash-loop` when a container restarts
twice in ten minutes and `deploy-failed` while a Deployment is past its progress deadline. An
automatic update that fails reaches the same channels, so the updater sends nothing of its own.
Not covered: a message that names the update and its rollback, and a StatefulSet that never gets
ready without crashing. Both need a rule and an event in Zeabur Catalog's rule set.

## Tests and what they do not show

- `zeabur-plan`: the `mutation.imageUpdate`, `mutation.imageFollow` and `plan-image-updates-*`
  cases (window edges, daylight saving time, ownership, attempts, rollouts) and
  `tests/image_update.rs` (stale writes, records written over a change).
- `zeabur` (binary): passes over the in-memory cluster with the real API: an update, a crash loop
  the rollout report calls stuck, the report's rollback applied, the record.
- `zeabur-api`: opting in through a plan, the service's view, an unknown time zone refused; the
  registry's tag list read page by page.
- Not yet run on a real cluster: the admission policy's CEL, the RBAC, Docker Hub's tag lists and
  a real rollout.
