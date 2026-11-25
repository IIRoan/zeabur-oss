import { describe, expect, test } from "bun:test"

import { translator } from "@/i18n"
import { en } from "@/i18n/en"

import { defaultWindow, lastWords, parseWindow, windowText, windowWords } from "./image-updates"

const t = translator(en)

describe("image update windows", () => {
  test("the annotation and the form's fields are the same four parts", () => {
    const window = parseWindow("mon,thu 23:30 90m Europe/Amsterdam")
    expect(window).toEqual({
      days: "mon,thu",
      start: "23:30",
      length: "90m",
      zone: "Europe/Amsterdam",
    })
    expect(window && windowText(window)).toBe("mon,thu 23:30 90m Europe/Amsterdam")
    expect(parseWindow("sun 03:00 2h")).toBeUndefined()
    expect(windowText(defaultWindow("UTC"))).toBe("sun 03:00 2h UTC")
  })

  test("a window reads as words with its days, length and zone", () => {
    expect(windowWords(t, defaultWindow("Europe/Amsterdam"))).toBe(
      "every Sunday at 03:00 for 2 hours (Europe/Amsterdam)",
    )
    expect(windowWords(t, { days: "daily", start: "01:00", length: "1h", zone: "UTC" })).toBe(
      "every day at 01:00 for 1 hour (UTC)",
    )
    expect(windowWords(t, { days: "mon,thu", start: "23:30", length: "90m", zone: "UTC" })).toBe(
      "every Monday and Thursday at 23:30 for 90m (UTC)",
    )
  })

  test("the last update says how it ended", () => {
    const when = (iso: string) => iso.slice(0, 10)
    expect(lastWords(t, "cache", {}, when)).toBeUndefined()
    expect(lastWords(t, "cache", { checked: "2026-11-29T02:00:00Z" }, when)).toBe(
      "No newer patch release on 2026-11-29.",
    )
    const rolledBack = { state: "rolledBack", from: "redis:7.2.4", to: "redis:7.2.5" } as const
    expect(
      lastWords(t, "cache", { ...rolledBack, failed: "7.2.5", reason: "crashLoop" }, when),
    ).toBe(
      "The rollout of cache is stuck: a new replica keeps crashing. redis:7.2.4 is back, and 7.2.5 won't be tried again.",
    )
    expect(lastWords(t, "cache", { ...rolledBack, reason: "not done by the deadline" }, when)).toBe(
      "cache's new replicas weren't ready in time. redis:7.2.4 is back, and redis:7.2.5 won't be tried again.",
    )
  })
})
