import type { Schemas } from "@/api/client"
import { type Translate, writeSentence } from "@/i18n"

/** A window as the form edits it: days (`daily` or `mon,thu`), the local start (`03:00`), how long
 * it stays open (`2h`) and an IANA time zone. The annotation is the four, space-separated. */
export interface WindowParts {
  days: string
  start: string
  length: string
  zone: string
}

export const defaultWindow = (zone: string): WindowParts => ({
  days: "sun",
  start: "03:00",
  length: "2h",
  zone,
})

export function parseWindow(text: string): WindowParts | undefined {
  const [days, start, length, zone, ...rest] = text.trim().split(/\s+/)
  if (!days || !start || !length || !zone || rest.length > 0) return undefined
  return { days, start, length, zone }
}

export const windowText = (w: WindowParts): string =>
  [w.days, w.start, w.length, w.zone.trim()].join(" ")

const DAYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] as const

/** The weekday's name, Monday first; 2024-01-01 was a Monday. */
function weekdayName(t: Translate, day: string): string {
  const at = DAYS.indexOf(day as (typeof DAYS)[number])
  return at < 0 ? day : t("imageUpdates.weekday", at)
}

/** The days a window can open on: every day, one weekday, or the list it already names. */
export function dayChoices(t: Translate, days: string): { value: string; label: string }[] {
  const choices = [
    { value: "daily", label: t("imageUpdates.daily") },
    ...DAYS.map((day) => ({ value: day, label: weekdayName(t, day) })),
  ]
  return choices.some((c) => c.value === days)
    ? choices
    : [...choices, { value: days, label: days }]
}

/** How long a window can stay open: a few hours, or the length it already has. */
export function lengthChoices(t: Translate, length: string): { value: string; label: string }[] {
  return [...new Set(["1h", "2h", "4h", "8h", length])].map((value) => {
    const hours = /^(\d+)h$/.exec(value)?.[1]
    return { value, label: hours ? t("imageUpdates.hours", Number(hours)) : value }
  })
}

/** The window in words: "every Sunday at 03:00 for 2 hours (Europe/Amsterdam)". */
export function windowWords(t: Translate, w: WindowParts): string {
  const days =
    w.days === "daily"
      ? t("imageUpdates.everyDay")
      : t(
          "imageUpdates.every",
          t(
            "common.list",
            w.days.split(",").map((d) => weekdayName(t, d)),
          ),
        )
  const hours = /^(\d+)h$/.exec(w.length)?.[1]
  const length = hours ? t("imageUpdates.hours", Number(hours)) : w.length
  return t("imageUpdates.window", days, w.start, length, w.zone)
}

/** Why a rollout failed, in the rollout report's words for its reason. */
function stuckWords(t: Translate, name: string, reason: string): string {
  return (
    writeSentence(t, { key: `rollout.stuck.${reason}`, params: { name } }) ??
    t("imageUpdates.late", name)
  )
}

/** What the image updater last did to `name`; none before its first check. */
export function lastWords(
  t: Translate,
  name: string,
  last: Schemas.ImageUpdateRecord,
  when: (iso: string) => string,
): string | undefined {
  const to = last.to ?? ""
  const from = last.from ?? ""
  const reason = last.reason ?? ""
  switch (last.state) {
    case "updating":
      return t("imageUpdates.updating", to)
    case "updated":
      return t("imageUpdates.updated", from, to, last.at ? when(last.at) : "")
    case "rolledBack":
      return t("common.sentences", [
        stuckWords(t, name, reason),
        t("imageUpdates.rolledBack", from, last.failed ?? to),
      ])
    case "failed":
      return t("imageUpdates.failed", to, reason)
    case "superseded":
      return t("imageUpdates.superseded", to)
    case undefined:
    case null:
      return last.checked ? t("imageUpdates.upToDate", when(last.checked)) : undefined
  }
}
