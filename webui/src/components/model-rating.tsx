import { useId, useMemo, useState, type FormEvent } from "react";
import { CircleAlert, CircleCheck, CircleDashed, Loader2, RefreshCw, TriangleAlert, Trash2, X } from "lucide-react";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { formatLocalDateTime } from "@/lib/format";
import {
  isValidRatingPrefix,
  modelMatchesPrefix,
  parseRatingScore,
  prefixCoverage,
  type ModelCatalogSnapshot,
  type RatingDisplayState,
  type RatingLoadState,
} from "@/lib/model-ratings";
import { useModelRatingMutations, useModelRatings } from "@/lib/use-model-ratings";
import type { ModelRatingEntry, Provider } from "@/lib/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";

/**
 * Open the editor for an existing entry, or prefill a new one from a concrete
 * model name (which the user can shorten inside the dialog).
 */
export interface ModelRatingEditTarget {
  initialPrefix: string;
  entry: ModelRatingEntry | null;
}

export function ModelRatingBadge({ state }: { state: RatingDisplayState }) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  if (state.status === "rated") {
    return (
      <Badge
        variant="secondary"
        className="gap-1 tabular-nums"
        title={isZh
          ? `前缀 ${state.entry.model_prefix} · ${formatLocalDateTime(state.entry.updated_at)}`
          : `Prefix ${state.entry.model_prefix} · ${formatLocalDateTime(state.entry.updated_at)}`}
      >
        <span className="text-xs font-semibold">{state.entry.score}</span>
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
        <p>{isZh ? "评分已清除。" : "Rating cleared."}</p>
        <p className="mt-1 whitespace-pre-wrap break-all font-mono text-xs">{target.initialPrefix}</p>
      </div>
      <Button variant="ghost" size="icon" className="h-6 w-6 shrink-0" aria-label={isZh ? "关闭提示" : "Dismiss notice"} onClick={onDismiss}><X className="h-4 w-4" /></Button>
    </div>
  );
}

/**
 * Unified prefix editor. The prefix itself is editable — shortening it widens
 * the match, and the live preview shows exactly which provider models the entry
 * would cover, plus overlaps with other entries.
 */
export function ModelRatingEditor({ target, providers, catalogs, onClose, onCleared }: {
  target: ModelRatingEditTarget;
  providers: Provider[];
  catalogs: ModelCatalogSnapshot[];
  onClose: () => void;
  onCleared: (target: ModelRatingEditTarget) => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const prefixId = useId();
  const scoreId = useId();
  const [prefixDraft, setPrefixDraft] = useState(target.initialPrefix);
  const [scoreDraft, setScoreDraft] = useState(target.entry ? String(target.entry.score) : "");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const { save, clear } = useModelRatingMutations();
  const ratings = useModelRatings();
  const pending = save.isPending || clear.isPending;
  const validationMessage = isZh ? "请输入 0 至 100 的整数，不能为空或包含小数。" : "Enter a whole number from 0 to 100. Empty values and fractions are not valid.";
  const prefixMessage = isZh ? "前缀不能为空白，不超过 1024 字节。" : "The prefix cannot be blank and must stay within 1024 bytes.";

  const preview = useMemo(() => {
    if (!isValidRatingPrefix(prefixDraft)) return null;
    const coverage = prefixCoverage(prefixDraft, catalogs, providers);
    const modelCount = coverage.reduce((total, group) => total + group.models.length, 0);
    // Entries strictly shorter than the draft would be shadowed by it. The
    // canonical (lowercased) comparison excludes the entry being edited.
    const draftKey = prefixDraft.toLowerCase();
    const shadowed = (ratings.data ?? []).filter((entry) => entry.model_prefix.toLowerCase() !== draftKey
      && coverage.some((group) => group.models.some((model) => modelMatchesPrefix(model, entry.model_prefix))));
    return { coverage, modelCount, shadowed };
  }, [prefixDraft, catalogs, providers, ratings.data]);

  const prefixChanged = prefixDraft.toLowerCase() !== (target.entry?.model_prefix ?? target.initialPrefix).toLowerCase();

  function handleSave(event: FormEvent) {
    event.preventDefault();
    if (pending) return;
    const score = parseRatingScore(scoreDraft);
    if (!isValidRatingPrefix(prefixDraft) || score === null) {
      setValidationError(!isValidRatingPrefix(prefixDraft) ? "prefix" : "score");
      return;
    }
    setValidationError(null);
    save.mutate({ modelPrefix: prefixDraft, score }, {
      onSuccess: (saved) => {
        // Editing the prefix moves the entry: remove the old canonical key.
        if (prefixChanged && target.entry && target.entry.model_prefix !== saved.model_prefix) {
          clear.mutate({ modelPrefix: target.entry.model_prefix }, { onSuccess: onClose });
        } else {
          onClose();
        }
      },
    });
  }

  return (
    <>
      <Dialog open onOpenChange={(open) => { if (!open && !pending && !confirmClear) onClose(); }}>
        <DialogContent showCloseButton={!pending} className="max-h-[90vh] overflow-y-auto">
          <DialogHeader>
            <DialogTitle>{isZh ? "编辑前缀评分" : "Edit prefix rating"}</DialogTitle>
            <DialogDescription>{isZh ? "前缀跨所有供应商共享：模型名等于前缀、或以“前缀-”开头的模型都会命中此分数，匹配忽略大小写（保存时统一转为小写）。评分不影响路由。" : "A prefix is shared by every provider: models equal to it or continuing after \"prefix-\" match this score. Matching ignores case (prefixes are stored lowercase). Ratings do not affect routing."}</DialogDescription>
          </DialogHeader>
          <form onSubmit={handleSave} noValidate className="space-y-3">
            <div className="space-y-1.5">
              <label htmlFor={prefixId} className="text-xs font-medium text-slate-700">{isZh ? "模型前缀" : "Model prefix"}</label>
              <Input
                id={prefixId}
                type="text"
                autoComplete="off"
                autoFocus
                value={prefixDraft}
                disabled={pending}
                aria-invalid={validationError === "prefix"}
                aria-describedby={`${prefixId}-help`}
                onChange={(event) => { setPrefixDraft(event.target.value); setValidationError(null); }}
                className="font-mono"
                placeholder="deepseek-v4-pro"
              />
              <p id={`${prefixId}-help`} className="text-xs text-slate-500">{isZh ? "示例：前缀 deepseek-v4-pro 命中 deepseek-v4-pro、DeepSeek-V4-Pro-0813，但不命中 deepseek-v4-pro2。匹配忽略大小写，多条命中时最长前缀胜出。" : "Example: prefix deepseek-v4-pro matches deepseek-v4-pro and DeepSeek-V4-Pro-0813, but not deepseek-v4-pro2. Matching ignores case; longest prefix wins on overlaps."}</p>
              {validationError === "prefix" && <p role="alert" className="text-xs text-red-600">{prefixMessage}</p>}
            </div>
            <div className="space-y-1.5">
              <label htmlFor={scoreId} className="text-xs font-medium text-slate-700">{isZh ? "综合评分（0–100）" : "Comprehensive score (0–100)"}</label>
              <Input
                id={scoreId}
                type="text"
                inputMode="numeric"
                autoComplete="off"
                value={scoreDraft}
                disabled={pending}
                aria-invalid={validationError === "score"}
                onChange={(event) => { setScoreDraft(event.target.value); setValidationError(null); }}
                placeholder="0–100"
              />
              <p className="text-xs text-slate-500">{isZh ? "0 是有效评分。清空输入不会删除评分，请使用“清除评分”。" : "0 is a valid score. An empty input does not delete a rating; use Clear rating."}</p>
              {validationError === "score" && <p role="alert" className="text-xs text-red-600">{validationMessage}</p>}
            </div>
            <div className="rounded-xl bg-slate-50 p-3">
              <p className="flex items-center gap-1.5 text-xs font-medium text-slate-700">
                <CircleCheck className="h-3.5 w-3.5 text-teal-600" />
                {preview
                  ? (isZh
                    ? `将命中 ${preview.modelCount} 个已知模型（${preview.coverage.length} 个供应商）`
                    : `${preview.modelCount} known models match (${preview.coverage.length} providers)`)
                    : prefixMessage}
              </p>
              <ul className="mt-2 max-h-44 space-y-1.5 overflow-y-auto text-xs text-slate-600">
                {preview?.coverage.map((group) => (
                  <li key={group.provider.id} className="break-all">
                    <span className="font-medium text-slate-800">{group.provider.name}</span>
                    <span className="text-slate-400"> · </span>
                    {group.models.map((model) => (
                      <span key={model} className="font-mono after:content-[',_'] last:after:content-['']">{model}</span>
                    ))}
                  </li>
                ))}
                {preview && preview.coverage.length === 0 && (
                  <li className="flex items-center gap-1.5 text-amber-600"><TriangleAlert className="h-3.5 w-3.5" />{isZh ? "当前没有已知模型命中此前缀。" : "No known model matches this prefix yet."}</li>
                )}
              </ul>
              {preview && preview.shadowed.length > 0 && (
                <p className="mt-2 flex items-start gap-1.5 text-xs text-amber-600">
                  <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                  {isZh ? "这些已命中的模型同时被更短的前缀覆盖，本条目（更长）优先：" : "These matched models are also covered by shorter prefixes; this (longer) entry wins: "}
                  <span className="break-all font-mono">{preview.shadowed.map((entry) => entry.model_prefix).join(", ")}</span>
                </p>
              )}
            </div>
            {save.error && <p role="alert" className="break-words text-xs text-red-600">{isZh ? "保存失败：" : "Save failed: "}{localizeBackendErrorMessage(save.error, isZh)}</p>}
            {clear.error && <p role="alert" className="break-words text-xs text-red-600">{isZh ? "迁移旧前缀失败，请手动删除：" : "Moving the old prefix failed; delete it manually: "}{localizeBackendErrorMessage(clear.error, isZh)}</p>}
            <DialogFooter className="flex-wrap">
              <Button type="button" variant="ghost" disabled={pending || !target.entry} onClick={() => setConfirmClear(true)} className="mr-auto text-red-600 hover:text-red-700">
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
            <DialogDescription>{isZh ? "删除此前缀的已保存评分。清除后为未评分，而不是 0 分。" : "Delete this prefix's saved score. It becomes unrated, not a score of 0."}</DialogDescription>
          </DialogHeader>
          <p className="mt-3 whitespace-pre-wrap break-all font-mono text-sm text-slate-600">{target.entry?.model_prefix}</p>
          {clear.error && <p role="alert" className="mt-3 break-words text-xs text-red-600">{isZh ? "清除失败：" : "Clear failed: "}{localizeBackendErrorMessage(clear.error, isZh)}</p>}
          <DialogFooter>
            <Button variant="secondary" disabled={pending} onClick={() => setConfirmClear(false)}>{isZh ? "取消" : "Cancel"}</Button>
            <Button className="bg-red-600 text-white hover:bg-red-500" disabled={pending} onClick={() => { if (!pending) clear.mutate({ modelPrefix: target.entry!.model_prefix }, { onSuccess: () => { onCleared(target); onClose(); } }); }}>
              {clear.isPending && <Loader2 className="h-4 w-4 animate-spin" />}{isZh ? "确认清除" : "Confirm clear"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
