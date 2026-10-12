import { useEffect, useState } from "react";
import { Loader2 } from "lucide-react";
import type { OAuthSessionStatusData } from "@/lib/types";
import { localizeBackendErrorMessage } from "@/lib/backend-error";

export function OAuthSessionRemaining({ expiresAt, isZh }: { expiresAt: number | null; isZh: boolean }) {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (!expiresAt) return;
    let timer: ReturnType<typeof setTimeout>;
    const tick = () => {
      setNow(Date.now());
      timer = setTimeout(tick, 1000);
    };
    tick();
    return () => clearTimeout(timer);
  }, [expiresAt]);
  if (!expiresAt) return null;
  const remaining = Math.max(0, Math.ceil((expiresAt - now) / 1000));
  return <p>{isZh ? `剩余时间：${remaining} 秒` : `Time remaining: ${remaining}s`}</p>;
}

export function OAuthProgress({ expiresAt, retryCount, pending, userCode, isZh }: {
  expiresAt: number | null;
  retryCount: number;
  pending: boolean;
  userCode?: string;
  isZh: boolean;
}) {
  return (
    <div className="space-y-2 text-xs text-slate-600" role="status">
      <div className="flex items-center gap-2 font-medium">
        {pending && <Loader2 className="h-4 w-4 animate-spin" />}
        {pending
          ? (isZh ? "等待浏览器完成授权…" : "Waiting for browser authorization…")
          : (isZh ? "开始授权后，将自动检查结果。" : "Start authorization to check the result automatically.")}
      </div>
      <p>{isZh ? "无需粘贴回调地址或授权码。" : "No callback URL or authorization code needs to be pasted here."}</p>
      {pending && userCode && <p>{isZh ? "在授权页输入此验证码：" : "Enter this code on the authorization page: "}<strong className="select-all">{userCode}</strong></p>}
      {pending && <OAuthSessionRemaining expiresAt={expiresAt} isZh={isZh} />}
      {pending && retryCount > 0 && <p className="text-amber-700">{isZh ? `网络暂时不可用，正在重试（${retryCount}/3）…` : `Connection temporarily unavailable. Retrying (${retryCount}/3)…`}</p>}
    </div>
  );
}

export function OAuthSessionError({ status, isZh }: { status: OAuthSessionStatusData | null; isZh: boolean }) {
  if (status?.status !== "error") return null;
  const message = isZh && ["OAUTH_TIMEOUT", "AUTH_TIMEOUT"].includes(status.code)
    ? "授权会话已超时，请重新发起授权。"
    : isZh && status.code === "OAUTH_CANCELLED"
      ? "授权已取消，可重新开始。"
      : localizeBackendErrorMessage(status.message, isZh);
  return <p role="alert" className="text-xs text-rose-600">{status.code}: {message}</p>;
}

export function CursorExperimentalNotice({ isZh }: { isZh: boolean }) {
  return (
    <div className="col-span-2 space-y-1 rounded-lg border border-amber-200 bg-amber-50 px-3 py-2 text-xs text-amber-800">
      <p className="font-semibold">{isZh ? "Cursor 原生接入（实验性）" : "Cursor native provider (experimental)"}</p>
      <p>{isZh
        ? "仅支持文本和串行工具调用，不支持图像。系统指令仅尽力保留，无法保证与原生 system 角色完全一致。用量未知，请勿将其视为零用量。"
        : "Text and serial tool calls only; images are not supported. System instructions are best effort, not guaranteed native system-role semantics. Usage is unknown, not zero."}</p>
      <p>{isZh
        ? "使用固定的原生 HTTP/2 传输和浏览器授权，不支持 API Key 或自定义协议端点。"
        : "Uses fixed native HTTP/2 transport and browser authorization, not API keys or custom protocol endpoints."}</p>
    </div>
  );
}
