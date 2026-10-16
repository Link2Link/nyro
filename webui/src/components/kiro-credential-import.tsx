import { useId, useState } from "react";

interface KiroCredentialImportProps {
  isZh: boolean;
  sessionId?: string | null;
  pending: boolean;
  busy: boolean;
  reimport?: boolean;
  onStart: () => void;
  onSubmit: (credential: string) => Promise<void>;
  onCancel: () => void;
}

/** Import-only auth: raw credentials live only in this mounted form, never a
 * provider draft, query cache or browser storage. The caller keys it by session. */
export function KiroCredentialImport({
  isZh, sessionId, pending, busy, reimport = false, onStart, onSubmit, onCancel,
}: KiroCredentialImportProps) {
  const id = useId();
  const [credential, setCredential] = useState("");
  const [visible, setVisible] = useState(false);
  const buttonClass = "rounded-md border border-slate-300 bg-white px-3 py-2 text-xs font-medium disabled:cursor-not-allowed disabled:opacity-50";

  async function submit() {
    if (!pending || busy || !sessionId || !credential.trim()) return;
    const pasted = credential.trim();
    // The hook owns the request. Do not retain another copy after submission,
    // including when an exchange fails or a ready session awaits creation.
    setCredential("");
    setVisible(false);
    await onSubmit(pasted);
  }

  return (
    <div className="col-span-2 space-y-3 rounded-xl border border-slate-200 bg-slate-50 p-4">
      <p className="text-sm font-semibold text-slate-800">
        {reimport
          ? (isZh ? "重新导入 Kiro 凭证" : "Re-import Kiro credential")
          : (isZh ? "导入 Kiro 凭证" : "Import Kiro credential")}
      </p>
      <p className="text-xs text-slate-600">
        {isZh
          ? "先在 Kiro IDE / CLI 登录，然后复制凭证文件内容。这里只导入已有凭证，不会打开浏览器或等待回调。Builder ID / IAM Identity Center 请同时粘贴客户端注册 JSON。"
          : "Sign in to the Kiro IDE / CLI first, then copy the credential file contents. This form imports an existing credential; there is no browser sign-in or callback. For Builder ID / IAM Identity Center, also paste the client-registration JSON."}
      </p>
      {reimport && <p className="text-xs text-slate-500">
        {isZh ? "导入成功后才会替换现有授权。" : "The existing authorization is replaced only after a successful import."}
      </p>}
      {!sessionId || !pending ? (
        <button type="button" className={buttonClass} onClick={onStart} disabled={busy}>
          {busy ? (isZh ? "初始化中…" : "Initializing…") : (isZh ? "开始导入" : "Start import")}
        </button>
      ) : (
        <>
          <div className="flex items-center justify-between gap-2">
            <label htmlFor={id} className="text-xs text-slate-700">
              {isZh ? "凭证 JSON / refresh token / ksk_ API Key" : "Credential JSON / refresh token / ksk_ API key"}
            </label>
            <button type="button" className={buttonClass} aria-controls={id} aria-pressed={visible} onClick={() => setVisible((value) => !value)} disabled={busy}>
              {visible ? (isZh ? "隐藏凭证" : "Hide credential") : (isZh ? "显示凭证" : "Show credential")}
            </button>
          </div>
          <div className="relative">
            <textarea
              id={id}
              className={`w-full rounded-md border border-slate-300 bg-white px-3 py-2 font-mono text-xs caret-slate-800 placeholder:text-slate-400 ${visible ? "text-slate-800" : "text-transparent selection:text-transparent"}`}
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="none"
              spellCheck={false}
              rows={5}
              placeholder={isZh ? "粘贴 kiro-auth-token.json，可在后面追加注册 JSON；也支持 refresh token 或 ksk_…" : "Paste kiro-auth-token.json, optionally followed by registration JSON; or a refresh token / ksk_…"}
              value={credential}
              onChange={(event) => setCredential(event.target.value)}
              disabled={busy}
              aria-describedby={`${id}-privacy`}
            />
            {!visible && credential && <span aria-hidden="true" className="pointer-events-none absolute inset-x-3 top-3 text-xs text-slate-500">
              {isZh ? "凭证已隐藏" : "Credential hidden"}
            </span>}
          </div>
          <p id={`${id}-privacy`} className="text-xs text-slate-500">
            {isZh ? "输入内容默认隐藏，不保存到浏览器；提交、取消或关闭表单时清除。" : "Hidden by default and never saved in the browser. Cleared on submit, cancel or closing the form."}
          </p>
          <div className="flex flex-wrap gap-2">
            <button type="button" className={buttonClass} onClick={() => void submit()} disabled={busy || !credential.trim()}>
              {busy ? (isZh ? "导入中…" : "Importing…") : (isZh ? "导入凭证" : "Import credential")}
            </button>
          </div>
        </>
      )}
      <button type="button" className={buttonClass} onClick={() => { setCredential(""); setVisible(false); onCancel(); }}>
        {isZh ? "取消" : "Cancel"}
      </button>
    </div>
  );
}
