import { useId, useState, type FormEvent } from "react";
import { CircleAlert, CircleDashed, Loader2, RefreshCw, Trash2, X } from "lucide-react";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { formatLocalDateTime } from "@/lib/format";
import { countRatingOverrides, hasModelRating, parseRatingProfileDraft, parseRatingScore, ratingDimensionLabel, ratingDisplayState, ratingProfileDraft, type RatingDisplayState, type RatingLoadState } from "@/lib/model-ratings";
import { useModelRatingMutations } from "@/lib/use-model-ratings";
import { EFFORT_TIERS, type EffortTier, type ProviderModelRatingProfile } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";

export interface ModelRatingEditTarget {
  providerId: string;
  providerName: string;
  model: string;
  rating: ProviderModelRatingProfile | null;
}

export function ModelRatingBadge({ state }: { state: RatingDisplayState }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  if (state.status === "rated") {
    const overrides = countRatingOverrides(state.rating);
    return (
      <Badge variant="secondary" className="gap-1 tabular-nums" title={state.rating.common ? formatLocalDateTime(state.rating.common.updated_at) : undefined}>
        {state.rating.common ? <><span>{isZh ? "通用" : "Common"}</span><span className="text-xs font-semibold">{state.rating.common.score}</span><span className="text-slate-400">/ 100</span></> : <span>{isZh ? "仅分档评分" : "Per-effort only"}</span>}
        {overrides > 0 && <span>· {isZh ? `${overrides} 项覆盖` : `${overrides} override${overrides === 1 ? "" : "s"}`}</span>}
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

export function ModelRatingProfileFields({ profile }: { profile: ProviderModelRatingProfile }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  return <dl className="mt-2 space-y-1 text-xs text-slate-500">
    <div><dt className="inline">{isZh ? "显示模式：" : "Display mode: "}</dt><dd className="inline">{profile.display_mode === "common" ? (isZh ? "通用" : "Common") : (isZh ? "分档" : "Per effort")}</dd></div>
    <div><dt className="inline">{isZh ? "通用：" : "Common: "}</dt><dd className="inline">{profile.common ? `${profile.common.score} / 100 · ${formatLocalDateTime(profile.common.updated_at)}` : (isZh ? "未设置" : "Unset")}</dd></div>
    {EFFORT_TIERS.map((tier) => {
      const value = profile.effective[tier];
      const source = value.source === "override" ? (isZh ? "覆盖" : "override") : value.source === "common" ? (isZh ? "通用" : "common") : (isZh ? "未评分" : "unrated");
      return <div key={tier}><dt className="inline font-medium">{ratingDimensionLabel(tier, isZh)}: </dt><dd className="inline">{value.score === null ? (isZh ? "未评分" : "Unrated") : `${value.score} / 100`} · {source}{value.score_updated_at && <> · <time dateTime={value.score_updated_at}>{formatLocalDateTime(value.score_updated_at)}</time></>}</dd></div>;
    })}
  </dl>;
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
  const [draft, setDraft] = useState(() => ratingProfileDraft(target.rating));
  const [validationError, setValidationError] = useState(false);
  const [confirmClear, setConfirmClear] = useState(false);
  const { save, clear } = useModelRatingMutations();
  const pending = save.isPending || clear.isPending;
  const identity = { providerId: target.providerId, model: target.model };
  const validationMessage = isZh ? "请输入 0 至 100 的整数，不能为空或包含小数。" : "Enter a whole number from 0 to 100. Empty values and fractions are not valid.";

  function updateOverride(tier: EffortTier, patch: Partial<typeof draft.overrides[EffortTier]>) {
    setDraft((previous) => ({ ...previous, overrides: { ...previous.overrides, [tier]: { ...previous.overrides[tier], ...patch } } }));
    setValidationError(false);
  }

  function preview(tier: EffortTier) {
    const override = draft.overrides[tier];
    const enabled = override.enabled || draft.commonEnabled;
    const score = parseRatingScore(override.enabled ? override.score : draft.common);
    if (!enabled) return isZh ? "未评分 · 来源：未评分" : "Unrated · source: unrated";
    if (score === null) return isZh ? "输入无效，无法预览" : "Invalid score; preview unavailable";
    const source = override.enabled ? (isZh ? "覆盖" : "override") : (isZh ? "通用" : "common");
    return `${score} / 100 · ${isZh ? "来源" : "source"}: ${source}`;
  }

  function handleSave(event: FormEvent) {
    event.preventDefault();
    if (pending) return;
    const input = parseRatingProfileDraft(draft);
    setValidationError(input === null);
    if (input === null) return;
    if (hasModelRating(target.rating) && input.common === null && EFFORT_TIERS.every((tier) => input.overrides[tier] === null)) {
      setConfirmClear(true);
      return;
    }
    save.mutate({ ...identity, input }, { onSuccess: () => {
      if (input.common === null && EFFORT_TIERS.every((tier) => input.overrides[tier] === null)) onCleared(target);
      onClose();
    } });
  }

  return (
    <>
      <Dialog open onOpenChange={(open) => { if (!open && !pending && !confirmClear) onClose(); }}>
        <DialogContent showCloseButton={!pending} className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
          <DialogHeader>
            <DialogTitle>{isZh ? "编辑模型评分" : "Edit model rating"}</DialogTitle>
            <DialogDescription>{isZh ? "为此供应商的确切模型设置可选通用评分及五档覆盖，一次保存整个配置。评分不影响路由。" : "Set an optional common score and five effort overrides for this exact provider/model. All fields save atomically. Ratings do not affect routing."}</DialogDescription>
          </DialogHeader>
          <div className="my-4 rounded-xl bg-slate-50 p-3 text-sm">
            <p className="font-medium text-slate-800">{target.providerName}</p>
            <p className="mt-1 break-all text-xs text-slate-400">{target.providerId}</p>
            <p className="mt-2 whitespace-pre-wrap break-all font-mono text-slate-700">{target.model}</p>
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <ModelRatingBadge state={ratingDisplayState("ready", target.rating)} />
            </div>
          </div>
          <form onSubmit={handleSave} noValidate>
            <fieldset disabled={pending} className="space-y-3">
              <legend className="text-sm font-medium text-slate-700">{isZh ? "评分配置（0–100）" : "Rating profile (0–100)"}</legend>
              <div className="rounded-lg border border-slate-200 p-3">
                <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={draft.commonEnabled} onChange={(event) => { setDraft((previous) => ({ ...previous, commonEnabled: event.target.checked })); setValidationError(false); }} />{isZh ? "设置通用评分" : "Set common score"}</label>
                <label htmlFor={inputId} className="sr-only">{isZh ? "通用评分" : "Common score"}</label>
                <Input id={inputId} type="text" inputMode="numeric" autoComplete="off" value={draft.common} disabled={pending || !draft.commonEnabled} aria-invalid={validationError && draft.commonEnabled && parseRatingScore(draft.common) === null} onChange={(event) => { setDraft((previous) => ({ ...previous, common: event.target.value })); setValidationError(false); }} className="mt-2" placeholder="0–100" />
                <p className="mt-2 text-xs text-slate-500">{isZh ? "取消通用评分不会删除分档覆盖。0 是有效评分。" : "Unsetting common does not remove overrides. 0 is a valid score."}</p>
              </div>
              {EFFORT_TIERS.map((tier) => <div key={tier} className="rounded-lg border border-slate-200 p-3">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <span className="text-sm font-medium">{ratingDimensionLabel(tier, isZh)}</span>
                  <div className="flex gap-3 text-xs">
                    <label className="flex items-center gap-1"><input type="radio" name={`${inputId}-${tier}-mode`} aria-label={`${ratingDimensionLabel(tier, isZh)}: ${isZh ? "继承通用" : "Inherit common"}`} checked={!draft.overrides[tier].enabled} onChange={() => updateOverride(tier, { enabled: false })} />{isZh ? "继承通用" : "Inherit common"}</label>
                    <label className="flex items-center gap-1"><input type="radio" name={`${inputId}-${tier}-mode`} aria-label={`${ratingDimensionLabel(tier, isZh)}: ${isZh ? "覆盖" : "Override"}`} checked={draft.overrides[tier].enabled} onChange={() => updateOverride(tier, { enabled: true })} />{isZh ? "覆盖" : "Override"}</label>
                  </div>
                </div>
                <label htmlFor={`${inputId}-${tier}`} className="sr-only">{ratingDimensionLabel(tier, isZh)} {isZh ? "覆盖评分" : "override score"}</label>
                <Input id={`${inputId}-${tier}`} className="mt-2" type="text" inputMode="numeric" autoComplete="off" value={draft.overrides[tier].score} disabled={pending || !draft.overrides[tier].enabled} aria-invalid={validationError && draft.overrides[tier].enabled && parseRatingScore(draft.overrides[tier].score) === null} placeholder="0–100" onChange={(event) => updateOverride(tier, { score: event.target.value })} />
                <p className="mt-2 text-xs text-slate-500" aria-live="polite">{isZh ? "保存后预览：" : "After save: "}{preview(tier)}</p>
              </div>)}
            </fieldset>
            {validationError && <p role="alert" className="mt-2 text-xs text-red-600">{validationMessage}</p>}
            {save.error && <p role="alert" className="mt-2 break-words text-xs text-red-600">{isZh ? "保存失败：" : "Save failed: "}{localizeBackendErrorMessage(save.error, isZh)}</p>}
            <DialogFooter className="flex-wrap">
              <Button type="button" variant="ghost" disabled={pending || !hasModelRating(target.rating)} onClick={() => setConfirmClear(true)} className="mr-auto text-red-600 hover:text-red-700">
                <Trash2 className="h-4 w-4" />{isZh ? "清除整个评分配置" : "Clear whole profile"}
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
            <DialogTitle>{isZh ? "确认清除整个评分配置？" : "Clear the whole rating profile?"}</DialogTitle>
            <DialogDescription>{isZh ? "一次清除此供应商、此模型的通用评分及全部五档覆盖。清除后全部为未评分，而不是 0 分。此操作独立于当前草稿。" : "Clear the common score and all five overrides for this provider/model in one operation, independently of the current draft. Every tier becomes unrated, not 0."}</DialogDescription>
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
