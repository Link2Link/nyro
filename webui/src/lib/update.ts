/** Server self-update: API types and pure presentation helpers. */

export interface SelfUpdateSupport {
  supported: boolean;
  /** disabled | unsupported_os | data_dir_not_writable | unavailable */
  reason: string | null;
}

export interface VersionInfo {
  version: string;
  platform: string;
  update_check: boolean;
  self_update: SelfUpdateSupport;
}

export interface UpdateCheck {
  current_version: string;
  latest_version: string;
  update_available: boolean;
  notes: string;
  pub_date: string | null;
  release_url: string | null;
  platform: string;
  asset_available: boolean;
  download_size: number | null;
  can_apply: boolean;
  checked_at: number;
}

export type UpdatePhase = "idle" | "downloading" | "verifying" | "installing" | "restarting" | "failed";

export interface UpdateJob {
  phase: UpdatePhase;
  target_version: string | null;
  downloaded_bytes: number;
  total_bytes: number | null;
  message: string | null;
  updated_at: number;
}

const ACTIVE_PHASES: readonly UpdatePhase[] = ["downloading", "verifying", "installing", "restarting"];

export function isJobActive(job: Pick<UpdateJob, "phase"> | null | undefined): boolean {
  return Boolean(job && ACTIVE_PHASES.includes(job.phase));
}

/** Download progress in whole percent, or null when the size is unknown. */
export function progressPercent(job: Pick<UpdateJob, "downloaded_bytes" | "total_bytes">): number | null {
  if (!job.total_bytes || job.total_bytes <= 0) return null;
  const ratio = job.downloaded_bytes / job.total_bytes;
  return Math.max(0, Math.min(100, Math.floor(ratio * 100)));
}

export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || !Number.isFinite(bytes) || bytes < 0) return "–";
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** Date part of an RFC 3339 timestamp; empty for missing or malformed input. */
export function formatReleaseDate(value: string | null | undefined): string {
  const match = value?.match(/^(\d{4}-\d{2}-\d{2})/);
  return match ? match[1] : "";
}

export function phaseLabel(phase: UpdatePhase, isZh: boolean): string {
  const labels: Record<UpdatePhase, [string, string]> = {
    idle: ["Idle", "空闲"],
    downloading: ["Downloading", "正在下载"],
    verifying: ["Verifying signature and binary", "正在校验签名与文件"],
    installing: ["Installing", "正在安装"],
    restarting: ["Restarting the gateway", "正在重启网关"],
    failed: ["Update failed", "更新失败"],
  };
  const [en, zh] = labels[phase] ?? labels.idle;
  return isZh ? zh : en;
}

/** Why in-place install is unavailable; null when it is available. */
export function selfUpdateBlockReason(
  info: Pick<VersionInfo, "self_update"> | null | undefined,
  check: Pick<UpdateCheck, "asset_available" | "platform"> | null | undefined,
  isZh: boolean,
): string | null {
  if (!info) return null;
  if (!info.self_update.supported) {
    const reasons: Record<string, [string, string]> = {
      disabled: [
        "In-place updates are disabled on this instance (NYRO_SELF_UPDATE=false).",
        "此实例已关闭在线更新（NYRO_SELF_UPDATE=false）。",
      ],
      unsupported_os: [
        "In-place updates are not supported on this operating system.",
        "当前操作系统不支持在线更新。",
      ],
      data_dir_not_writable: [
        "The data directory is not writable, so the new version cannot be installed.",
        "数据目录不可写，无法安装新版本。",
      ],
    };
    const [en, zh] = reasons[info.self_update.reason ?? ""] ?? [
      "In-place updates are unavailable on this instance.",
      "此实例无法在线更新。",
    ];
    return isZh ? zh : en;
  }
  if (check && !check.asset_available) {
    return isZh
      ? `该版本没有适用于 ${check.platform} 的安装包，请手动升级。`
      : `This release has no build for ${check.platform}; upgrade manually.`;
  }
  return null;
}

/** Returned when the latest release carries no signed update manifest yet. */
export const UPDATE_NOT_PUBLISHED = "UPDATE_NOT_PUBLISHED";

/** Machine-readable code of an update API error, if any. */
export function updateErrorCode(error: unknown): string | undefined {
  if (error && typeof error === "object" && "code" in error) {
    const code = (error as { code?: unknown }).code;
    if (typeof code === "string") return code;
  }
  return undefined;
}

/** Neutral notice shown instead of an error when nothing is published yet. */
export function notPublishedNotice(isZh: boolean): string {
  return isZh
    ? "当前最新发布版本尚未提供在线更新包，发布带在线更新的新版本后即可在此一键更新。"
    : "The latest release does not offer an in-place update yet. New versions published with update support will appear here.";
}

/** Localize the update API's English error messages. */
export function localizeUpdateError(message: string, isZh: boolean): string {
  if (!isZh) return message;
  const prefixes: [string, string][] = [
    ["no installable update has been published yet", "当前最新发布版本尚未提供在线更新包"],
    ["failed to fetch update", "无法获取更新信息，请检查网络或代理设置"],
    ["update verification failed", "更新校验失败"],
    ["invalid update manifest", "更新清单无效"],
    ["an update is already in progress", "已有更新正在进行"],
    ["self-update is not available", "此实例无法在线更新"],
    ["requested version", "所选版本已不是最新版本，请重新检查更新"],
  ];
  for (const [prefix, zh] of prefixes) {
    if (message.startsWith(prefix)) {
      const detail = message.slice(prefix.length).replace(/^[:\s]+/, "");
      return detail && prefix !== "requested version" ? `${zh}：${detail}` : zh;
    }
  }
  return message;
}
