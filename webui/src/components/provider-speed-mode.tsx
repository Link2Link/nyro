import { useId } from "react";
import { supportsSpeedMode } from "../lib/provider-speed-mode";
import type { OpenAiSpeedMode } from "../lib/types";

interface ProviderSpeedModeProps {
  channel?: string | null;
  value?: OpenAiSpeedMode;
  onChange: (mode: OpenAiSpeedMode) => void;
  isZh: boolean;
  className?: string;
}

export function ProviderSpeedMode({ channel, value = "normal", onChange, isZh, className }: ProviderSpeedModeProps) {
  const id = useId();
  if (!supportsSpeedMode(channel)) return null;
  return (
    <div className={className}>
      <label htmlFor={id} className="text-sm font-medium text-slate-800">{isZh ? "速度模式" : "Speed mode"}</label>
      <select id={id} aria-describedby={`${id}-help`} value={value}
        onChange={(event) => onChange(event.target.value as OpenAiSpeedMode)}
        className="w-full rounded-md border border-slate-300 bg-white px-3 py-2 text-sm">
        <option value="normal">{isZh ? "普通" : "Normal"}</option>
        <option value="fast">Fast</option>
        <option value="ultrafast">Ultrafast</option>
      </select>
      <p id={`${id}-help`} className="text-xs text-slate-500">{isZh
        ? "对于发往 Codex 或 Sub2API 的 OpenAI Responses 请求，普通模式既不添加也不改写 service_tier。Fast 和 Ultrafast 分别将上游请求的 service_tier 强制设为 priority 和 ultrafast，即使客户端已指定档位。"
        : "For OpenAI Responses requests to Codex or Sub2API, Normal neither adds nor changes service_tier. Fast and Ultrafast force the upstream service_tier to priority and ultrafast, respectively, even if the client already supplied a tier."}</p>
    </div>
  );
}
