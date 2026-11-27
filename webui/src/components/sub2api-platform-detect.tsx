import { useState } from "react";
import { Loader2, ScanSearch } from "lucide-react";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { detectionKey, evidenceText, platformLabel, type Sub2ApiDetectionState } from "@/lib/sub2api";
import { detectSub2ApiPlatform } from "@/lib/sub2api-detect";
import type { ProviderChannelPreset } from "@/lib/types";
import { Button } from "@/components/ui/button";

const CONFIDENCE = {
  high: { zh: "高", en: "High", className: "bg-emerald-50 text-emerald-700" },
  medium: { zh: "中", en: "Medium", className: "bg-amber-50 text-amber-700" },
  low: { zh: "低", en: "Low", className: "bg-slate-100 text-slate-600" },
} as const;

/** "Detect platform" for the Sub2API preset form. Advisory: applying is explicit. */
export function Sub2ApiPlatformDetect({ address, apiKey, useProxy, channel, channels, detection, onDetected, onApply, isZh }: {
  address: string;
  apiKey: string;
  useProxy: boolean;
  /** Currently selected platform (channel id). */
  channel: string;
  channels: ProviderChannelPreset[];
  detection: Sub2ApiDetectionState | null;
  onDetected: (state: Sub2ApiDetectionState) => void;
  onApply: (platform: string) => void;
  isZh: boolean;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ready = Boolean(address.trim() && apiKey.trim());
  const current = detection && detection.key === detectionKey(address, apiKey) ? detection.result : null;
  const label = (id: string) => platformLabel(channels, id, isZh);
  const confidence = current ? CONFIDENCE[current.confidence] : null;
  const suggestions = current
    ? [...new Set([...(current.platform ? [current.platform] : []), ...current.candidates])]
      .filter((id) => channels.some((item) => item.id === id))
    : [];

  async function run() {
    setPending(true);
    setError(null);
    try {
      onDetected(await detectSub2ApiPlatform(address, apiKey, useProxy));
    } catch (cause) {
      setError(localizeBackendErrorMessage(cause, isZh));
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="col-span-2 space-y-2" data-sub2api-detect>
      <div className="flex flex-wrap items-center gap-3">
        <Button type="button" variant="secondary" size="sm" onClick={() => void run()} disabled={!ready || pending}>
          {pending ? <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" /> : <ScanSearch className="mr-1.5 h-3.5 w-3.5" />}
          {isZh ? "识别平台" : "Detect platform"}
        </Button>
        <span className="text-xs text-slate-500">
          {ready
            ? (isZh ? "读取该 Key 的模型列表和分组信息推断平台，不消耗额度。" : "Infers the platform from the key's model list and group, spending no quota.")
            : (isZh ? "先填写 Sub2API 地址和 API Key。" : "Enter the Sub2API address and API key first.")}
        </span>
      </div>
      {error && <p role="alert" className="text-xs text-red-600">{error}</p>}
      {current && (
        <div className="space-y-2 rounded-lg border border-slate-200 bg-slate-50 px-3 py-2.5 text-xs text-slate-600" data-sub2api-detection>
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-medium text-slate-800">
              {current.platform
                ? (isZh ? `建议平台：${label(current.platform)}` : `Suggested platform: ${label(current.platform)}`)
                : (isZh ? "无法确定平台" : "Platform is ambiguous")}
            </span>
            {confidence && (
              <span className={`rounded px-1.5 py-0.5 text-[10px] font-semibold ${confidence.className}`}>
                {isZh ? `置信度${confidence.zh}` : `${confidence.en} confidence`}
              </span>
            )}
            {current.platform === channel && (
              <span className="text-emerald-700">{isZh ? "与当前选择一致" : "Matches the selected platform"}</span>
            )}
          </div>
          {suggestions.some((id) => id !== channel) && (
            <div className="flex flex-wrap items-center gap-2">
              <span>{isZh ? "使用：" : "Use:"}</span>
              {suggestions.filter((id) => id !== channel).map((id) => (
                <Button key={id} type="button" size="sm" variant="outline" className="h-7 px-2 text-xs" onClick={() => onApply(id)}>
                  {label(id)}
                </Button>
              ))}
            </div>
          )}
          <ul className="list-disc space-y-0.5 pl-4">
            {current.evidence.map((line) => <li key={line}>{evidenceText(line, isZh)}</li>)}
          </ul>
          <p className="text-slate-500">
            {current.group_name && <>{isZh ? "分组：" : "Group: "}{current.group_name} · </>}
            {isZh ? `${current.model_count} 个模型` : `${current.model_count} models`}
            {current.sample_models.length > 0 && <>{isZh ? "：" : ": "}<span className="break-all font-mono">{current.sample_models.join(", ")}</span></>}
          </p>
        </div>
      )}
    </div>
  );
}
