import { equal, notEqual } from "node:assert/strict";
import { test } from "node:test";
import { accountUsagePeriodStart } from "./account-usage-stats";

test("weekly usage starts at local Monday midnight, including Sunday and year boundaries", () => {
  equal(accountUsagePeriodStart("week", new Date(2026, 9, 25, 23, 59).getTime()), new Date(2026, 9, 19).getTime());
  equal(accountUsagePeriodStart("week", new Date(2026, 9, 26).getTime()), new Date(2026, 9, 26).getTime());
  equal(accountUsagePeriodStart("week", new Date(2027, 0, 1, 12).getTime()), new Date(2026, 11, 28).getTime());
});

test("monthly usage starts at local first-of-month midnight across year boundaries", () => {
  equal(accountUsagePeriodStart("month", new Date(2026, 9, 31, 23, 59).getTime()), new Date(2026, 9, 1).getTime());
  equal(accountUsagePeriodStart("month", new Date(2027, 0, 1).getTime()), new Date(2027, 0, 1).getTime());
  notEqual(accountUsagePeriodStart("month", new Date(2026, 11, 31, 23, 59).getTime()), accountUsagePeriodStart("month", new Date(2027, 0, 1).getTime()));
});

test("calendar boundaries respect browser timezone and DST rather than fixed-duration subtraction", () => {
  const previous = process.env.TZ;
  process.env.TZ = "America/New_York";
  try {
    equal(accountUsagePeriodStart("week", Date.parse("2026-03-08T16:00:00Z")), Date.parse("2026-03-02T05:00:00Z"));
    equal(accountUsagePeriodStart("month", Date.parse("2026-03-30T16:00:00Z")), Date.parse("2026-03-01T05:00:00Z"));
  } finally {
    if (previous === undefined) delete process.env.TZ;
    else process.env.TZ = previous;
  }
});
