import { useState } from "react";
import { useLocale } from "@/lib/i18n";
import { localizeBackendErrorMessage } from "@/lib/backend-error";
import { bindKiroAccountSession, initKiroAccountSession, kiroLoginConfigured } from "@/lib/kiro-login";
import { looksLikeCredential } from "@/lib/kiro-pool-usage";
import { openExternalUrl } from "@/lib/open-external";
import { useOAuthSession, type OAuthSessionIntent } from "@/lib/use-oauth-session";
import type { KiroLoginOptions } from "@/lib/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { KiroLoginOptionsForm } from "@/components/kiro-login-options";
import { KiroCredentialImport } from "@/components/kiro-credential-import";
import { OAuthProgress, OAuthSessionError, OAuthSessionRemaining } from "@/components/oauth-session-status";

/** Reuses the Kiro browser/import flow, with a backend-owned pool account target. */
export function KiroAccountLogin({ intent, initialLabel = "", onBound, onCancel }: {
  intent: OAuthSessionIntent;
  initialLabel?: string;
  onBound: () => void;
  onCancel: () => void;
}) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";
  const oauth = useOAuthSession();
  const [options, setOptions] = useState<KiroLoginOptions>({ method: "google" });
  const [label, setLabel] = useState(initialLabel);
  const [error, setError] = useState("");
  const legacy = intent.kind === "reauthorize_legacy_provider";
  const reauth = intent.kind !== "add_account";
  const importing = options.method === "import";
  const unsafeLabel = looksLikeCredential(label);
  const active = Boolean(oauth.session) && oauth.status?.status !== "error";
  const authUrl = active && (oauth.session?.auth_url || oauth.session?.verification_uri_complete || oauth.session?.verification_uri);

  async function start() {
    if (!kiroLoginConfigured(options) || unsafeLabel) return;
    setError("");
    // Immutable label/options/target snapshots: controls cannot retarget a session.
    const boundLabel = label.trim() || undefined;
    await oauth.start({
      vendor: "kiro", useProxy: legacy ? intent.useProxy : false, intent,
      init: () => initKiroAccountSession(intent, options),
      onReady: async (_status, context) => {
        if (context.intent?.providerId !== intent.providerId) return;
        if (!await bindKiroAccountSession(context, boundLabel)) return;
        oauth.reset(false);
        onBound();
      },
    });
  }

  function cancel() {
    oauth.reset();
    setError("");
    onCancel();
  }

  return <section className="space-y-3 rounded-lg border border-sky-200 bg-sky-50/40 p-3">
    <h4 className="text-sm font-semibold">{reauth ? (isZh ? "重新授权此 Kiro 账号" : "Re-authorize this Kiro account") : (isZh ? "添加独立 Kiro 账号" : "Add a separate Kiro account")}</h4>
    <p className="text-xs text-slate-600">{legacy
      ? (isZh ? "保留原单账号登录，不会创建账号池或添加第二个账号。" : "Keep the legacy single-account sign-in flow without creating a pool or adding a second account.")
      : reauth
        ? (isZh ? "仅在授权成功后替换所选账号的凭据，其他账号不受影响。版本已过期时请关闭并重试。" : "Only the selected account's credentials are replaced after successful authorization. Other accounts are unaffected. Close and retry if its version has changed.")
        : (isZh ? "追加账号，不覆盖现有授权。" : "Append an account without replacing existing authorizations.")}</p>
    {!legacy && <label className="block space-y-1 text-xs"><span>{isZh ? "安全别名（可选，不要填写令牌）" : "Safe label (optional; do not enter a token)"}</span>
      <Input value={label} disabled={active || oauth.busy} maxLength={120} aria-invalid={unsafeLabel || undefined} onChange={(event) => setLabel(event.target.value)} />
      {unsafeLabel && <span role="alert" className="block text-red-600">{isZh ? "别名看起来包含凭证，请改为普通名称；凭证请粘贴到下方导入框" : "This label looks like a credential; use a plain name and paste credentials into the import box below"}</span>}
    </label>}
    <KiroLoginOptionsForm value={options} isZh={isZh} disabled={oauth.status?.status === "ready"}
      onChange={(value) => { oauth.reset(); setError(""); setOptions(value); }} />
    {importing ? <>
      <KiroCredentialImport key={oauth.session?.session_id ?? "kiro-pool-import"} isZh={isZh} reimport={reauth}
        sessionId={oauth.session?.session_id} pending={oauth.status?.status === "pending"} busy={oauth.busy}
        onStart={() => void start()} onSubmit={(code) => oauth.complete({ code })} onCancel={cancel} />
      <OAuthSessionRemaining expiresAt={oauth.expiresAt} isZh={isZh} />
    </> : <>
      {!active && <Button type="button" disabled={oauth.busy || unsafeLabel || !kiroLoginConfigured(options)} onClick={() => void start()}>{isZh ? "开始授权" : "Start authorization"}</Button>}
      {authUrl && <div className="space-y-2 text-xs">
        <p className="select-all break-all">{authUrl}</p>
        <Button type="button" variant="secondary" onClick={() => {
          const sessionId = oauth.session?.session_id;
          void openExternalUrl(authUrl).catch((cause) => {
            if (sessionId && oauth.ownsSession(sessionId)) setError(localizeBackendErrorMessage(cause, isZh));
          });
        }}>{isZh ? "打开授权页" : "Open authorization page"}</Button>
      </div>}
      {oauth.status?.status === "pending" && <OAuthProgress isZh={isZh} expiresAt={oauth.expiresAt} retryCount={oauth.retryCount} pending userCode={oauth.status.user_code} />}
      <Button type="button" variant="secondary" onClick={cancel}>{isZh ? "取消" : "Cancel"}</Button>
    </>}
    <OAuthSessionError status={oauth.status} isZh={isZh} />
    {error && <p role="alert" className="text-xs text-red-600">{error}</p>}
  </section>;
}
