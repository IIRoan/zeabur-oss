---
default: minor
---

# Services can move to new patch releases of their image on their own

A service's **Settings** can turn on automatic updates with a window, such as Sundays at 03:00 for two hours in Europe/Amsterdam. In that window Zeabur OSS moves the image to the newest patch release of its version (`redis:7.2.4` to `redis:7.2.5`), follows the rollout, and rolls back a stuck one; that version is not tried again, and a new minor or major version is shown as waiting for you. A cluster administrator turns the updater on with the chart's `imageUpdates.enabled`; workloads that Helm, Argo CD, Flux, an operator or Keel manage are not updated. A failed rollout reaches the project's notification channels as any other does.
