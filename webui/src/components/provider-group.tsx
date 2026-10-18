import { useId, type ReactNode } from "react";
import { AlertCircle, ChevronRight, Layers } from "lucide-react";
import { ProviderIcon } from "@/components/ui/provider-icon";
import type { ProviderGroup as Group } from "@/lib/provider-groups";

export function ProviderGroup({ group, expanded, searching, editing, issueCount, isZh, onToggle, onShowIssues, children }: {
  group: Group;
  expanded: boolean;
  searching: boolean;
  editing: boolean;
  issueCount: number;
  isZh: boolean;
  onToggle: () => void;
  onShowIssues: () => void;
  children: ReactNode;
}) {
  const id = useId();
  const enabled = group.providers.filter((provider) => provider.is_enabled).length;
  return (
    <section className="glass rounded-2xl">
      <div className="flex min-h-16 flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2">
        <button
          type="button"
          className="flex min-w-0 flex-1 items-center gap-3 rounded-lg py-2 text-left outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:cursor-default"
          aria-expanded={expanded}
          aria-controls={id}
          disabled={editing || searching}
          title={editing ? (isZh ? "请先保存或取消编辑" : "Save or cancel editing first") : undefined}
          onClick={onToggle}
        >
          <ChevronRight className={`h-4 w-4 shrink-0 text-slate-400 transition-transform duration-200 motion-reduce:transition-none ${expanded ? "rotate-90" : ""}`} />
          <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-slate-100">
            {group.key === "custom" ? <Layers className="h-5 w-5 text-slate-500" /> : <>
              <ProviderIcon iconKey={group.key} name={group.label} size={26} className="provider-preset-icon provider-preset-icon-colored" />
              <ProviderIcon iconKey={group.key} name={group.label} size={26} monochrome className="provider-preset-icon provider-preset-icon-mono" />
            </>}
          </span>
          <span className="min-w-0 truncate font-semibold text-slate-900">{group.label}</span>
          <span className="shrink-0 rounded-md bg-slate-100 px-2 py-0.5 text-xs tabular-nums text-slate-500">
            {searching ? (isZh ? `匹配 ${group.matches.length} / 共 ${group.providers.length}` : `${group.matches.length} / ${group.providers.length} matches`) : (isZh ? `${group.providers.length} 个配置` : `${group.providers.length} configs`)}
          </span>
        </button>
        <div className="flex items-center gap-3 pl-7 text-xs sm:pl-0">
          <span className="text-slate-500">{isZh ? "已启用" : "Enabled"} <span className="tabular-nums">{enabled}/{group.providers.length}</span></span>
          {issueCount > 0 && <button type="button" onClick={onShowIssues} className="inline-flex items-center gap-1 rounded-md px-2 py-1 text-orange-600 hover:bg-orange-50 focus-visible:ring-2 focus-visible:ring-ring" title={isZh ? "查看最近测试失败或授权异常的配置" : "View failed tests or authorization issues"}>
            <AlertCircle className="h-3.5 w-3.5" />
            {isZh ? `${issueCount} 项异常` : `${issueCount} issues`}
          </button>}
        </div>
      </div>
      <div id={id} hidden={!expanded} className="divide-y divide-slate-200/60 border-t border-slate-200/60">
        {expanded ? children : null}
      </div>
    </section>
  );
}
