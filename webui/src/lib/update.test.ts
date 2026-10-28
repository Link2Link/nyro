import { equal } from "node:assert/strict";
import { test } from "node:test";
import {
  formatBytes,
  formatReleaseDate,
  isJobActive,
  localizeUpdateError,
  phaseLabel,
  progressPercent,
  selfUpdateBlockReason,
  type VersionInfo,
} from "./update";

const supported: VersionInfo = {
  version: "2.1.2",
  platform: "linux-x86_64",
  update_check: true,
  self_update: { supported: true, reason: null },
};

test("only in-flight phases count as an active job", () => {
  for (const phase of ["downloading", "verifying", "installing", "restarting"] as const) {
    equal(isJobActive({ phase }), true, phase);
  }
  for (const phase of ["idle", "failed"] as const) {
    equal(isJobActive({ phase }), false, phase);
  }
  equal(isJobActive(null), false);
});

test("progress is clamped whole percent and unknown without a size", () => {
  equal(progressPercent({ downloaded_bytes: 0, total_bytes: 200 }), 0);
  equal(progressPercent({ downloaded_bytes: 199, total_bytes: 200 }), 99);
  equal(progressPercent({ downloaded_bytes: 200, total_bytes: 200 }), 100);
  equal(progressPercent({ downloaded_bytes: 500, total_bytes: 200 }), 100);
  equal(progressPercent({ downloaded_bytes: 10, total_bytes: null }), null);
  equal(progressPercent({ downloaded_bytes: 10, total_bytes: 0 }), null);
});

test("byte and date formatting tolerate missing values", () => {
  equal(formatBytes(44_017_992), "42.0 MB");
  equal(formatBytes(84), "1 KB");
  equal(formatBytes(null), "–");
  equal(formatReleaseDate("2026-11-01T08:00:00Z"), "2026-11-01");
  equal(formatReleaseDate("soon"), "");
  equal(formatReleaseDate(null), "");
});

test("phase labels default to English and localize to Chinese", () => {
  equal(phaseLabel("downloading", false), "Downloading");
  equal(phaseLabel("downloading", true), "正在下载");
  equal(phaseLabel("failed", false), "Update failed");
});

test("block reason explains why in-place install is unavailable", () => {
  const check = { asset_available: true, platform: "linux-x86_64" };
  equal(selfUpdateBlockReason(supported, check, false), null);
  equal(selfUpdateBlockReason(undefined, check, false), null);

  const disabled = { ...supported, self_update: { supported: false, reason: "disabled" } };
  equal(selfUpdateBlockReason(disabled, check, false)?.includes("NYRO_SELF_UPDATE=false"), true);
  const readOnly = { ...supported, self_update: { supported: false, reason: "data_dir_not_writable" } };
  equal(selfUpdateBlockReason(readOnly, check, true), "数据目录不可写，无法安装新版本。");
  const unknown = { ...supported, self_update: { supported: false, reason: "something-new" } };
  equal(selfUpdateBlockReason(unknown, check, false), "In-place updates are unavailable on this instance.");

  const noAsset = { asset_available: false, platform: "linux-riscv64" };
  equal(selfUpdateBlockReason(supported, noAsset, false)?.includes("linux-riscv64"), true);
});

test("update errors stay English by default and localize known prefixes", () => {
  const fetchError = "failed to fetch update: error sending request";
  equal(localizeUpdateError(fetchError, false), fetchError);
  equal(
    localizeUpdateError(fetchError, true),
    "无法获取更新信息，请检查网络或代理设置：error sending request",
  );
  equal(
    localizeUpdateError("requested version 2.2.0 does not match the latest release 2.3.0", true),
    "所选版本已不是最新版本，请重新检查更新",
  );
  equal(localizeUpdateError("an update is already in progress", true), "已有更新正在进行");
  equal(localizeUpdateError("something else", true), "something else");
});
