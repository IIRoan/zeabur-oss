import { Button } from "@zeabur-oss/design-system/components/button"
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@zeabur-oss/design-system/components/collapsible"
import { Input } from "@zeabur-oss/design-system/components/input"
import { Label } from "@zeabur-oss/design-system/components/label"
import { Select } from "@zeabur-oss/design-system/components/select"
import { Switch } from "@zeabur-oss/design-system/components/switch"
import { useId, useState } from "react"
import CaretRightIcon from "~icons/ph/caret-right"

import { ManagedTip } from "@/components/bits"
import { FormField } from "@/components/form-field"
import { useHereGate } from "@/components/gate"
import { useI18n, useT } from "@/i18n"
import { formatTime } from "@/lib/format"
import {
  dayChoices,
  defaultWindow,
  lastWords,
  lengthChoices,
  parseWindow,
  type WindowParts,
  windowText,
  windowWords,
} from "@/model/image-updates"
import type { Service } from "@/model/service"
import type { ServiceSettingsProps, ServiceSettingsSection } from "@/modules/service-settings"

/** The window's four fields. */
function WindowFields({
  form,
  set,
}: {
  form: WindowParts
  set: (part: Partial<WindowParts>) => void
}) {
  const t = useT()
  return (
    <div className="grid gap-3 sm:grid-cols-4">
      <FormField label={t("imageUpdates.days")}>
        <Select
          size="sm"
          value={form.days}
          onValueChange={(days) => set({ days })}
          items={dayChoices(t, form.days)}
        />
      </FormField>
      <FormField label={t("imageUpdates.start")}>
        <Input
          size="sm"
          type="time"
          value={form.start}
          onChange={(event) => set({ start: event.target.value })}
        />
      </FormField>
      <FormField label={t("imageUpdates.length")}>
        <Select
          size="sm"
          value={form.length}
          onValueChange={(length) => set({ length })}
          items={lengthChoices(t, form.length)}
        />
      </FormField>
      <FormField label={t("tasks.zone")} hint={t("imageUpdates.zoneHint")}>
        <Input
          size="sm"
          mono
          value={form.zone}
          autoComplete="off"
          onChange={(event) => set({ zone: event.target.value })}
        />
      </FormField>
    </div>
  )
}

/** What the updater last did and what waits for a person, under the switch. */
function LastUpdate({ service }: { service: Service }) {
  const t = useT()
  const { locale } = useI18n()
  const updates = service.imageUpdates
  if (!updates) return null
  const last = lastWords(t, service.name, updates.last, (iso) => formatTime(iso, locale))
  const awaiting = updates.last.awaiting
  return (
    <>
      {last ? <p className="type-meta text-muted-foreground">{last}</p> : null}
      {awaiting ? (
        <p className="type-meta text-muted-foreground">{t("imageUpdates.awaiting", awaiting)}</p>
      ) : null}
    </>
  )
}

/** Whether the image updater moves the service to new patch releases of its image, and when: the
 * workload's `zeabur.com/image-updates` label and `zeabur.com/image-update-window` annotation. */
function ImageUpdatesSetting({ service, plan }: ServiceSettingsProps) {
  const t = useT()
  const id = useId()
  const updates = service.imageUpdates
  const managed = service.provenance.tool ?? undefined
  const gate = useHereGate("changeServices")
  const workload = { kind: service.kind, name: service.name }
  const saved = updates ? parseWindow(updates.window) : undefined
  const [form, setForm] = useState<WindowParts>(
    () => saved ?? defaultWindow(Intl.DateTimeFormat().resolvedOptions().timeZone),
  )
  const set = (part: Partial<WindowParts>) => setForm((current) => ({ ...current, ...part }))
  const ask = (window?: string) =>
    plan({
      title: t("title.imageUpdates", service.name),
      changes: [{ op: "imageUpdates", workload, window }],
      service: workload,
    })
  const moved = saved !== undefined && windowText(saved) !== windowText(form)
  return (
    <div className="flex flex-col gap-2 border-t pt-3">
      <div className="flex items-start gap-3">
        <div className="flex min-w-0 flex-1 flex-col gap-0.5">
          <Label htmlFor={id}>{t("imageUpdates.switch")}</Label>
          <p className="type-meta text-muted-foreground">
            {saved ? t("imageUpdates.on", windowWords(t, saved)) : t("imageUpdates.hint")}
          </p>
          <LastUpdate service={service} />
        </div>
        <ManagedTip tool={managed} gate={gate}>
          <Switch
            id={id}
            checked={Boolean(updates)}
            disabled={Boolean(managed)}
            onCheckedChange={(enabled) => ask(enabled ? windowText(form) : undefined)}
          />
        </ManagedTip>
      </div>
      <WindowFields form={form} set={set} />
      {moved ? (
        <div>
          <Button size="sm" variant="outline" onClick={() => ask(windowText(form))}>
            {t("imageUpdates.saveWindow")}
          </Button>
        </div>
      ) : null}
      <Collapsible>
        <CollapsibleTrigger variant="disclosure">
          <CaretRightIcon aria-hidden="true" />
          {t("imageUpdates.how")}
        </CollapsibleTrigger>
        <CollapsibleContent>
          <p className="max-w-prose pt-1 type-meta text-muted-foreground">
            {t("imageUpdates.mechanism")}
          </p>
        </CollapsibleContent>
      </Collapsible>
    </div>
  )
}

const updatable = (service: Service) =>
  service.kind === "Deployment" || service.kind === "StatefulSet"

export const imageUpdatesSection: ServiceSettingsSection = {
  module: "image-updates",
  title: "imageUpdates.switch",
  applies: updatable,
  Section: ImageUpdatesSetting,
}
