import { useId, useState, type FormEvent } from "react";
import { CircleAlert, CircleDashed, Loader2, RefreshCw, Trash2, X } from "lucide-react";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { formatLocalDateTime } from "@/lib/format";
import { parseRatingScore, type RatingDisplayState, type RatingLoadState } from "@/lib/model-ratings";
import { useModelRatingMutations } from "@/lib/use-model-ratings";
import type { ProviderModelRating } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";

export interface ModelRatingEditTarget {
  providerId: string;
  providerName: string;
  model: string;
  rating: ProviderModelRating | null;
}

export function ModelRatingBadge({ state }: { state: RatingDisplayState }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  if (state.status === "rated") {
    return (
      <Badge variant="secondary" className="gap-1 tabular-nums" title={formatLocalDateTime(state.rating.updated_at)}>
        <span className="text-xs font-semibold">{state.rating.score}</span>
        <span className="text-slate-400">/ 100</span>
      </Badge>
    );
  }
  if (state.status === "loading") {
    return <Badge variant="outline" className="gap-1 text-slate-500"><Loader2 className="h-3 w-3 animate-spin" />{isZh ? "加载评分中" : "Loading rating"}</Badge>;
  }
  if (state.status === "error") {
    return <Badge variant="danger" className="gap-1"><CircleAlert className="h-3 w-3" />{isZh ? "评分未知" : "Rating unknown"}</Badge>;
  }
  return <Badge variant="outline" className="gap-1 text-slate-500"><CircleDashed className="h-3 w-3" />{isZh ? "未评分" : "Unrated"}</Badge>;
}

export function ModelRatingsFeedback({ state, error, fetching, onRetry }: {
  state: RatingLoadState;
  error: unknown;
  fetching: boolean;
  onRetry: () => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  if (state === "ready") return null;
  if (state === "loading") {
    return <div role="status" className="flex items-center gap-2 rounded-xl bg-slate-50 px-4 py-3 text-sm text-slate-500"><Loader2 className="h-4 w-4 animate-spin" />{isZh ? "正在加载评分，尚无法确定评分状态。" : "Loading ratings; rating states are not known yet."}</div>;
  }
  return (
    <div role="alert" className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-red-200 bg-red-50/70 px-4 py-3">
      <div className="min-w-0 text-sm text-red-600">
        <p>{isZh ? "评分加载失败。不能将未知状态视为未评分，请重试。" : "Ratings could not be loaded. Unknown states are not unrated; please retry."}</p>
        <p className="mt-1 break-words text-xs">{localizeBackendErrorMessage(error, isZh)}</p>
      </div>
      <Button variant="secondary" size="sm" disabled={fetching} onClick={onRetry}>
        <RefreshCw className={fetching ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"} />{isZh ? "重试评分" : "Retry ratings"}
      </Button>
    </div>
  );
}

export function ModelRatingClearedNotice({ target, onDismiss }: { target: ModelRatingEditTarget; onDismiss: () => void }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  return (
    <div role="status" className="flex items-start justify-between gap-3 rounded-xl border border-green-200 bg-green-50/70 px-4 py-3 text-sm text-green-700">
      <div>
        <p>{isZh ? "评分已清除。若此模型仅来自评分记录，它将不再出现在列表中。" : "Rating cleared. A model known only from its saved rating no longer appears in the list."}</p>
        <p className="mt-1 whitespace-pre-wrap break-all font-mono text-xs">{target.providerName} / {target.model}</p>
      </div>
      <Button variant="ghost" size="icon" className="h-6 w-6 shrink-0" aria-label={isZh ? "关闭提示" : "Dismiss notice"} onClick={onDismiss}><X className="h-4 w-4" /></Button>
    </div>
  );
}

/** Mount only for the selected pair. Background refetches must never overwrite the draft. */
export function ModelRatingEditor({ target, onClose, onCleared }: {
  target: ModelRatingEditTarget;
  onClose: () => void;
  onCleared: (target: ModelRatingEditTarget) => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const inputId = useId();
  const [draft, setDraft] = useState(target.rating ? String(target.rating.score) : "");
  const [validationError, setValidationError] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);
  const { save, clear } = useModelRatingMutations();
  const pending = save.isPending || clear.isPending;
  const identity = { providerId: target.providerId, model: target.model };
  const validationMessage = isZh ? "请输入 0 至 100 的整数，不能为空或包含小数。" : "Enter a whole number from 0 to 100. Empty values and fractions are not valid.";

  function handleSave(event: FormEvent) {
    event.preventDefault();
    if (pending) return;
    const score = parseRatingScore(draft);
    setValidationError(score === null);
    if (score === null) return;
    save.mutate({ ...identity, score }, { onSuccess: onClose });
  }

  return (
    <>
      <Dialog open onOpenChange={(open) => { if (!open && !pending && !confirmClear) onClose(); }}>
        <DialogContent showCloseButton={!pending}>
          <DialogHeader>
            <DialogTitle>{isZh ? "编辑模型评分" : "Edit model rating"}</DialogTitle>
            <DialogDescription>{isZh ? "为此供应商的确切模型保存一个综合评分。评分不影响路由。" : "Save one comprehensive score for this exact provider and model. Ratings do not affect routing."}</DialogDescription>
          </DialogHeader>
          <div className="my-4 rounded-xl bg-slate-50 p-3 text-sm">
            <p className="font-medium text-slate-800">{target.providerName}</p>
            <p className="mt-1 break-all text-xs text-slate-400">{target.providerId}</p>
            <p className="mt-2 whitespace-pre-wrap break-all font-mono text-slate-700">{target.model}</p>
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <ModelRatingBadge state={target.rating ? { status: "rated", rating: target.rating } : { status: "unrated" }} />
              {target.rating && <time dateTime={target.rating.updated_at} className="text-xs text-slate-500">{formatLocalDateTime(target.rating.updated_at)}</time>}
            </div>
          </div>
          <form onSubmit={handleSave} noValidate>
            <label htmlFor={inputId} className="text-xs font-medium text-slate-700">{isZh ? "综合评分（0–100）" : "Comprehensive score (0–100)"}</label>
            <Input
              id={inputId}
              type="text"
              inputMode="numeric"
              autoComplete="off"
              autoFocus
              value={draft}
              disabled={pending}
              aria-invalid={validationError}
              aria-describedby={`${inputId}-help`}
              onChange={(event) => { setDraft(event.target.value); setValidationError(false); }}
              className="mt-1.5"
              placeholder="0–100"
            />
            <p id={`${inputId}-help`} className="mt-2 text-xs text-slate-500">{isZh ? "0 是有效评分。清空输入不会删除评分，请使用“清除评分”。" : "0 is a valid score. An empty input does not delete a rating; use Clear rating."}</p>
            {validationError && <p role="alert" className="mt-2 text-xs text-red-600">{validationMessage}</p>}
            {save.error && <p role="alert" className="mt-2 break-words text-xs text-red-600">{isZh ? "保存失败：" : "Save failed: "}{localizeBackendErrorMessage(save.error, isZh)}</p>}
            <DialogFooter className="flex-wrap">
              <Button type="button" variant="ghost" disabled={pending || !target.rating} onClick={() => setConfirmClear(true)} className="mr-auto text-red-600 hover:text-red-700">
                <Trash2 className="h-4 w-4" />{isZh ? "清除评分" : "Clear rating"}
              </Button>
              <Button type="button" variant="secondary" disabled={pending} onClick={onClose}>{isZh ? "取消" : "Cancel"}</Button>
              <Button type="submit" disabled={pending}>
                {save.isPending && <Loader2 className="h-4 w-4 animate-spin" />}{isZh ? "保存" : "Save"}
              </Button>
            </DialogFooter>
          </form>
        </DialogContent>
      </Dialog>
      <Dialog open={confirmClear} onOpenChange={(open) => { if (!pending) setConfirmClear(open); }}>
        <DialogContent showCloseButton={!pending}>
          <DialogHeader>
            <DialogTitle>{isZh ? "确认清除评分？" : "Clear this rating?"}</DialogTitle>
            <DialogDescription>{isZh ? "仅删除此供应商、此模型的已保存评分。清除后为未评分，而不是 0 分。" : "Delete only this provider/model's saved score. It becomes unrated, not a score of 0."}</DialogDescription>
          </DialogHeader>
          <p className="mt-3 whitespace-pre-wrap break-all font-mono text-sm text-slate-600">{target.providerName} / {target.model}</p>
          {clear.error && <p role="alert" className="mt-3 break-words text-xs text-red-600">{isZh ? "清除失败：" : "Clear failed: "}{localizeBackendErrorMessage(clear.error, isZh)}</p>}
          <DialogFooter>
            <Button variant="secondary" disabled={pending} onClick={() => setConfirmClear(false)}>{isZh ? "取消" : "Cancel"}</Button>
            <Button className="bg-red-600 text-white hover:bg-red-500" disabled={pending} onClick={() => { if (!pending) clear.mutate(identity, { onSuccess: () => { onCleared(target); onClose(); } }); }}>
              {clear.isPending && <Loader2 className="h-4 w-4 animate-spin" />}{isZh ? "确认清除" : "Confirm clear"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
