import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, Download, ExternalLink, Info, Loader2, RefreshCw, TriangleAlert } from "lucide-react";
import { backend } from "@/lib/backend";
import { openExternalUrl } from "@/lib/open-external";
import {
  formatBytes,
  formatReleaseDate,
  isJobActive,
  localizeUpdateError,
  phaseLabel,
  progressPercent,
  notPublishedNotice,
  selfUpdateBlockReason,
  UPDATE_NOT_PUBLISHED,
  updateErrorCode,
  type UpdateCheck,
  type UpdateJob,
  type VersionInfo,
} from "@/lib/update";
import { UPDATE_CHECK_KEY, useUpdateCheck, useVersionInfo } from "@/lib/use-update";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";

const RELEASES_URL = "https://github.com/Link2Link/nyro/releases";
const STATUS_POLL_MS = 1000;
const VERSION_POLL_MS = 2000;
const RESTART_TIMEOUT_MS = 3 * 60 * 1000;

const errorMessage = (error: unknown) => (error instanceof Error ? error.message : String(error));
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * Follow an update job until the gateway serves `target`, then reload so the
 * new embedded WebUI loads. The old process drops connections while it
 * restarts, so request failures after the install starts are expected.
 */
function useFollowUpdate(target: string | null, onFailed: (message: string) => void) {
  const [job, setJob] = useState<UpdateJob | null>(null);
  const [restarting, setRestarting] = useState(false);

  useEffect(() => {
    if (!target) return;
    let cancelled = false;
    const deadline = Date.now() + RESTART_TIMEOUT_MS;
    (async () => {
      let awaitingRestart = false;
      while (!cancelled && Date.now() < deadline) {
        if (!awaitingRestart) {
          try {
            const status = await backend<UpdateJob>("get_system_update_status");
            if (cancelled) return;
            setJob(status);
            if (status.phase === "failed") {
              onFailed(status.message ?? "update failed");
              return;
            }
            awaitingRestart = status.phase === "restarting" || !isJobActive(status);
          } catch {
            awaitingRestart = true;
          }
          if (awaitingRestart) setRestarting(true);
          else await sleep(STATUS_POLL_MS);
          continue;
        }
        try {
          const info = await backend<VersionInfo>("get_system_version");
          if (cancelled) return;
          if (info.version === target) {
            window.location.reload();
            return;
          }
        } catch {
          // Gateway still restarting.
        }
        await sleep(VERSION_POLL_MS);
      }
      if (!cancelled) onFailed("timeout");
    })();
    return () => {
      cancelled = true;
    };
    // onFailed is recreated each render; following depends only on target.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [target]);

  return { job, restarting };
}

export function UpdateCard({ isZh }: { isZh: boolean }) {
  const qc = useQueryClient();
  const { data: info } = useVersionInfo();
  const checkQuery = useUpdateCheck();
  const check = checkQuery.data;
  const [checking, setChecking] = useState(false);
  const [checkError, setCheckError] = useState<unknown>(null);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [target, setTarget] = useState<string | null>(null);
  const [flowError, setFlowError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  const { job, restarting } = useFollowUpdate(target, (message) => {
    setTarget(null);
    setFlowError(
      message === "timeout"
        ? isZh
          ? "网关重启后未在 3 分钟内恢复，请检查服务日志。"
          : "The gateway did not come back within 3 minutes; check the server logs."
        : localizeUpdateError(message, isZh),
    );
  });

  // Resume following a job started earlier (another tab or a page refresh).
  useEffect(() => {
    if (!info?.self_update.supported) return;
    backend<UpdateJob>("get_system_update_status")
      .then((status) => {
        if (isJobActive(status) && status.target_version) setTarget(status.target_version);
      })
      .catch(() => {});
  }, [info?.self_update.supported]);

  if (!info) return null;

  async function handleCheck() {
    setChecking(true);
    setCheckError(null);
    try {
      const fresh = await backend<UpdateCheck>("check_system_update", { force: true });
      qc.setQueryData(UPDATE_CHECK_KEY, fresh);
    } catch (error) {
      if (updateErrorCode(error) === UPDATE_NOT_PUBLISHED) {
        qc.removeQueries({ queryKey: UPDATE_CHECK_KEY });
      }
      setCheckError(error);
    } finally {
      setChecking(false);
    }
  }

  async function handleApply() {
    if (!check) return;
    setConfirmOpen(false);
    setFlowError(null);
    setStarting(true);
    try {
      await backend<UpdateJob>("apply_system_update", { version: check.latest_version });
      setTarget(check.latest_version);
    } catch (error) {
      setFlowError(localizeUpdateError(errorMessage(error), isZh));
    } finally {
      setStarting(false);
    }
  }

  const following = target !== null;
  const blockReason = selfUpdateBlockReason(info, check, isZh);
  const percent = job ? progressPercent(job) : null;
  const failure = checkError ?? checkQuery.error ?? null;
  // A release without a signed manifest is not a failure; show it neutrally.
  const notPublished = !check && updateErrorCode(failure) === UPDATE_NOT_PUBLISHED;
  const queryError =
    failure && !notPublished ? localizeUpdateError(errorMessage(failure), isZh) : null;
  const releaseUrl = check?.release_url ?? RELEASES_URL;

  return (
    <div className="glass rounded-2xl p-6 space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 className="text-lg font-semibold text-slate-900">{isZh ? "版本更新" : "Updates"}</h2>
        {info.update_check && (
          <Button
            variant="secondary"
            size="sm"
            className="flex items-center gap-1.5"
            disabled={checking || following}
            onClick={handleCheck}
          >
            {checking ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <RefreshCw className="h-3.5 w-3.5" />}
            {isZh ? "检查更新" : "Check for updates"}
          </Button>
        )}
      </div>

      <div className="grid grid-cols-2 gap-4 lg:grid-cols-4">
        <div className="rounded-xl bg-slate-50 p-4">
          <p className="text-xs text-slate-500">{isZh ? "当前版本" : "Current version"}</p>
          <p className="mt-1 font-semibold text-slate-900">{info.version}</p>
        </div>
        <div className="rounded-xl bg-slate-50 p-4">
          <p className="text-xs text-slate-500">{isZh ? "最新版本" : "Latest version"}</p>
          <p className="mt-1 flex items-center gap-2 font-semibold text-slate-900">
            {check?.latest_version ?? "–"}
            {check?.update_available && <Badge variant="warning">{isZh ? "可更新" : "New"}</Badge>}
          </p>
        </div>
        <div className="rounded-xl bg-slate-50 p-4">
          <p className="text-xs text-slate-500">{isZh ? "发布日期" : "Released"}</p>
          <p className="mt-1 font-semibold text-slate-900">{formatReleaseDate(check?.pub_date) || "–"}</p>
        </div>
        <div className="rounded-xl bg-slate-50 p-4">
          <p className="text-xs text-slate-500">{isZh ? "平台" : "Platform"}</p>
          <p className="mt-1 font-semibold text-slate-900">{info.platform}</p>
        </div>
      </div>

      {!info.update_check && (
        <p className="text-sm text-slate-500">
          {isZh ? "此实例已关闭更新检查（NYRO_UPDATE_CHECK=false）。" : "Update checks are disabled on this instance (NYRO_UPDATE_CHECK=false)."}
        </p>
      )}

      {queryError && !following && (
        <p role="alert" className="flex items-start gap-1.5 text-sm text-red-600">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" />
          {queryError}
        </p>
      )}

      {notPublished && !following && (
        <p className="flex items-start gap-1.5 text-sm text-slate-500">
          <Info className="mt-0.5 h-4 w-4 shrink-0" />
          {notPublishedNotice(isZh)}
        </p>
      )}

      {check && !check.update_available && !following && (
        <p className="flex items-center gap-1.5 text-sm text-green-600">
          <CheckCircle2 className="h-4 w-4" />
          {isZh ? "已是最新版本" : "You're on the latest version"}
        </p>
      )}

      {check?.update_available && (
        <div className="space-y-3 rounded-xl bg-slate-50 p-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-sm font-medium text-slate-900">
              {isZh ? `新版本 ${check.latest_version}` : `Version ${check.latest_version} is available`}
              {check.download_size != null && (
                <span className="ml-2 text-xs font-normal text-slate-500">{formatBytes(check.download_size)}</span>
              )}
            </p>
            <button
              type="button"
              className="flex items-center gap-1 text-xs text-slate-500 hover:text-slate-900"
              onClick={() => void openExternalUrl(releaseUrl)}
            >
              {isZh ? "发布页" : "Release page"}
              <ExternalLink className="h-3 w-3" />
            </button>
          </div>
          {check.notes && (
            <pre className="max-h-56 overflow-auto whitespace-pre-wrap break-words rounded-lg border border-slate-200 bg-white p-3 font-sans text-xs leading-relaxed text-slate-700">
              {check.notes}
            </pre>
          )}

          {following ? (
            <div className="space-y-2" aria-live="polite">
              <p className="flex items-center gap-1.5 text-sm text-slate-700">
                <Loader2 className="h-4 w-4 animate-spin" />
                {restarting
                  ? isZh
                    ? "正在重启网关，完成后页面将自动刷新…"
                    : "Restarting the gateway; this page reloads when it is back…"
                  : job
                    ? phaseLabel(job.phase, isZh)
                    : isZh
                      ? "正在准备更新…"
                      : "Preparing update…"}
                {!restarting && job?.phase === "downloading" && percent != null && (
                  <span className="text-xs text-slate-500">
                    {percent}% · {formatBytes(job.downloaded_bytes)} / {formatBytes(job.total_bytes)}
                  </span>
                )}
              </p>
              {!restarting && job?.phase === "downloading" && percent != null && (
                <div
                  className="h-2 overflow-hidden rounded-full bg-slate-200"
                  role="progressbar"
                  aria-valuemin={0}
                  aria-valuemax={100}
                  aria-valuenow={percent}
                  aria-label={isZh ? "下载进度" : "Download progress"}
                >
                  <div className="h-full rounded-full bg-slate-900 transition-all" style={{ width: `${percent}%` }} />
                </div>
              )}
            </div>
          ) : check.can_apply ? (
            <Button
              size="sm"
              className="flex items-center gap-1.5"
              disabled={starting}
              onClick={() => setConfirmOpen(true)}
            >
              {starting ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Download className="h-3.5 w-3.5" />}
              {isZh ? "立即更新" : "Update now"}
            </Button>
          ) : (
            blockReason && <p className="text-sm text-amber-600">{blockReason}</p>
          )}
        </div>
      )}

      {flowError && (
        <p role="alert" className="flex items-start gap-1.5 text-sm text-red-600">
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" />
          {flowError}
        </p>
      )}

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={isZh ? `更新到 ${check?.latest_version ?? ""}？` : `Update to ${check?.latest_version ?? ""}?`}
        description={
          isZh
            ? "网关会下载并校验新版本，然后自动重启（通常十几秒）。重启期间进行中的请求和流式响应会中断。版本只升不降，更新后无法回退。"
            : "The gateway downloads and verifies the new version, then restarts automatically (usually a few seconds). In-flight requests and streams are interrupted during the restart. Updates only move forward and cannot be undone."
        }
        cancelText={isZh ? "取消" : "Cancel"}
        confirmText={isZh ? "更新并重启" : "Update and restart"}
        confirmClassName="bg-slate-900 text-white hover:bg-slate-800"
        onConfirm={() => void handleApply()}
      />
    </div>
  );
}
