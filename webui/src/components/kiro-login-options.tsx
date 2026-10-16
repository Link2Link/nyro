import { useId } from "react";
import type { KiroLoginMethod, KiroLoginOptions } from "../lib/types";

interface KiroLoginOptionsProps {
  value: KiroLoginOptions;
  onChange: (value: KiroLoginOptions) => void;
  isZh: boolean;
  disabled?: boolean;
}

/** Only non-secret, login-scoped configuration. Changing it cancels the caller's session. */
export function KiroLoginOptionsForm({ value, onChange, isZh, disabled = false }: KiroLoginOptionsProps) {
  const id = useId();
  const browser = value.method !== "import";
  const oidc = value.method === "builder-id" || value.method === "idc";
  const fieldClass = "w-full rounded-md border border-slate-300 bg-white px-3 py-2 text-sm disabled:opacity-50";
  return (
    <div className="col-span-2 space-y-3 rounded-lg border border-slate-200 bg-slate-50 p-3">
      <div className="space-y-1">
        <label htmlFor={`${id}-method`} className="text-xs font-medium text-slate-700">{isZh ? "Kiro 登录方式" : "Kiro sign-in method"}</label>
        <select id={`${id}-method`} className={fieldClass} value={value.method} disabled={disabled}
          onChange={(event) => onChange({ method: event.target.value as KiroLoginMethod })}>
          <option value="google">{isZh ? "Google（浏览器登录）" : "Google (browser sign-in)"}</option>
          <option value="github">{isZh ? "GitHub（浏览器登录）" : "GitHub (browser sign-in)"}</option>
          <option value="builder-id">AWS Builder ID</option>
          <option value="idc">AWS IAM Identity Center</option>
          <option value="import">{isZh ? "导入已有凭证" : "Import existing credential"}</option>
        </select>
      </div>
      {browser && <p className="text-xs text-slate-600">{isZh
        ? "开始授权后打开登录链接。网关会自动检查授权结果，无需粘贴回调地址。也可在另一台设备打开链接；切换登录方式或配置会取消当前会话。"
        : "Start authorization, then open the sign-in link. The gateway checks the result automatically; no callback URL needs to be pasted. You can open the link on another device. Changing the method or configuration cancels the current session."}</p>}
      {value.method === "idc" && <div className="grid gap-3 md:grid-cols-2">
        <div className="space-y-1">
          <label htmlFor={`${id}-start`} className="text-xs text-slate-700">{isZh ? "组织 Start URL（必填）" : "Organization start URL (required)"}</label>
          <input id={`${id}-start`} type="url" className={fieldClass} placeholder="https://example.awsapps.com/start"
            value={value.start_url ?? ""} disabled={disabled} autoComplete="off" spellCheck={false}
            onChange={(event) => onChange({ ...value, start_url: event.target.value })} />
        </div>
        <div className="space-y-1">
          <label htmlFor={`${id}-idc-region`} className="text-xs text-slate-700">{isZh ? "Identity Center 区域（必填）" : "Identity Center region (required)"}</label>
          <input id={`${id}-idc-region`} className={fieldClass} placeholder="us-east-1"
            value={value.idc_region ?? ""} disabled={disabled} autoComplete="off" spellCheck={false}
            onChange={(event) => onChange({ ...value, idc_region: event.target.value })} />
        </div>
      </div>}
      {browser && <details className="space-y-2 text-xs text-slate-600">
        <summary className="cursor-pointer">{isZh ? "高级区域设置" : "Advanced region settings"}</summary>
        <div className="space-y-1">
          <label htmlFor={`${id}-region`}>{isZh ? "模型服务区域（默认 us-east-1）" : "Model service region (default: us-east-1)"}</label>
          <input id={`${id}-region`} className={fieldClass} placeholder="us-east-1" value={value.region ?? ""}
            disabled={disabled} autoComplete="off" spellCheck={false}
            onChange={(event) => onChange({ ...value, region: event.target.value })} />
        </div>
        {oidc && value.method !== "idc" && <div className="space-y-1">
          <label htmlFor={`${id}-oidc-region`}>{isZh ? "AWS OIDC 区域（可选）" : "AWS OIDC region (optional)"}</label>
          <input id={`${id}-oidc-region`} className={fieldClass} placeholder="us-east-1" value={value.idc_region ?? ""}
            disabled={disabled} autoComplete="off" spellCheck={false}
            onChange={(event) => onChange({ ...value, idc_region: event.target.value })} />
        </div>}
        <p>{isZh ? "模型服务区域与 Identity Center / OIDC 认证区域可以不同。" : "The model service region can differ from the Identity Center / OIDC authentication region."}</p>
      </details>}
    </div>
  );
}
