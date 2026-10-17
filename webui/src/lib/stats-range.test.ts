import { deepEqual, equal } from "node:assert/strict";
import { test } from "node:test";
import {
  DEFAULT_RANGE, fromDatetimeLocal, monthStart, rangeArgs, rangeKey, rangeSpanHours, toDatetimeLocal, validateCustomRange,
} from "./stats-range";

const HOUR = 3_600_000;
const NOW = new Date(2026, 4, 21, 15, 30).getTime(); // 2026-05-21 15:30 local

test("month starts at local midnight of the 1st", () => {
  equal(monthStart(NOW), new Date(2026, 4, 1).getTime());
  equal(monthStart(new Date(2026, 0, 1).getTime()), new Date(2026, 0, 1).getTime());
});

test("range args send hours for presets and bounds for month and custom", () => {
  deepEqual(rangeArgs(DEFAULT_RANGE, NOW), { hours: 24 });
  deepEqual(rangeArgs({ kind: "month" }, NOW), { startAt: monthStart(NOW) });
  deepEqual(rangeArgs({ kind: "custom", startAt: 5, endAt: null }, NOW), { startAt: 5 });
  deepEqual(rangeArgs({ kind: "custom", startAt: 5, endAt: 9 }, NOW), { startAt: 5, endAt: 9 });
});

test("query keys are stable per range and change with the month", () => {
  equal(rangeKey({ kind: "hours", hours: 6 }, NOW), "hours:6");
  equal(rangeKey({ kind: "month" }, NOW), rangeKey({ kind: "month" }, NOW + HOUR));
  const nextMonth = new Date(2026, 5, 2).getTime();
  equal(rangeKey({ kind: "month" }, NOW) === rangeKey({ kind: "month" }, nextMonth), false);
  equal(rangeKey({ kind: "custom", startAt: 1, endAt: null }, NOW), "custom:1:");
});

test("span in hours drives date labels on the axis", () => {
  equal(rangeSpanHours({ kind: "hours", hours: 72 }, NOW), 72);
  equal(rangeSpanHours({ kind: "custom", startAt: NOW - 5 * HOUR, endAt: null }, NOW), 5);
  equal(rangeSpanHours({ kind: "custom", startAt: NOW - 10 * HOUR, endAt: NOW - 4 * HOUR }, NOW), 6);
  equal(rangeSpanHours({ kind: "month" }, NOW) > 24, true);
});

test("custom range validation mirrors the server rules", () => {
  const day = 24 * HOUR;
  equal(validateCustomRange(null, null, NOW), "start_required");
  equal(validateCustomRange(NOW + HOUR, null, NOW), "start_future");
  equal(validateCustomRange(NOW - day, NOW - 2 * day, NOW), "order");
  equal(validateCustomRange(NOW - 400 * day, null, NOW), "too_long");
  equal(validateCustomRange(NOW - day, null, NOW), null);
  // A future end is clamped to now, like the server does.
  equal(validateCustomRange(NOW - day, NOW + day, NOW), null);
});

test("datetime-local values round-trip in local time", () => {
  const text = toDatetimeLocal(NOW);
  equal(text, "2026-05-21T15:30");
  equal(fromDatetimeLocal(text), NOW);
  equal(fromDatetimeLocal(""), null);
  equal(fromDatetimeLocal("not a date"), null);
});
