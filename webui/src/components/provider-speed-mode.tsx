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
        ? "普通模式不添加 service_tier。Fast 和 Ultrafast 分别设置为 fast 和 ultrafast。客户端已指定的 service_tier 不会被覆盖。"
        : "Normal does not add service_tier. Fast and Ultrafast set it to fast and ultrafast, respectively. A client-supplied service_tier is never overwritten."}</p>
    </div>
  );
}
