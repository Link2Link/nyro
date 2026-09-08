import { NavLink } from "react-router-dom";
import { cn } from "@/lib/utils";
import {
  LayoutDashboard,
  Boxes,
  Star,
  Route,
  Server,
  ScrollText,
  BarChart3,
  Activity,
  KeyRound,
  Plug,
  Puzzle,
  ChevronLeft,
  Settings,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { useLocale } from "@/lib/i18n";
import NyroLogo from "@/assets/logos/NYRO-logo.png";

const NAV_ITEMS = [
  { label: "Dashboard", path: "/", icon: LayoutDashboard },
  { label: "Providers", path: "/providers", icon: Server },
  { label: "Available Models", path: "/available-models", icon: Boxes },
  { label: "Model Ratings", path: "/model-ratings", icon: Star },
  { label: "Model Mapping", path: "/models", icon: Route },
  { label: "API Keys", path: "/api-keys", icon: KeyRound },
  { label: "Connect", path: "/connect", icon: Plug },
  { type: "divider" as const },
  { label: "Logs", path: "/logs", icon: ScrollText },
  { label: "Stats", path: "/stats", icon: BarChart3 },
  { label: "Performance", path: "/performance", icon: Activity },
  { label: "Extensions", path: "/extensions", icon: Puzzle },
  { type: "divider" as const },
  { label: "Settings", path: "/settings", icon: Settings },
] as const;

interface SidebarProps {
  collapsed: boolean;
  onToggle: () => void;
}

export function Sidebar({ collapsed, onToggle }: SidebarProps) {
  const { locale } = useLocale();
  const isZh = locale === "zh-CN";

  return (
    <aside
      className={cn(
        "sidebar-shell glass z-30 flex h-full shrink-0 flex-col rounded-[1.5rem] transition-all duration-300 ease-out",
        collapsed ? "w-[4.75rem]" : isZh ? "w-[11.5rem]" : "w-[12.25rem]"
      )}
    >
      {/* Logo */}
      <div className="flex h-[4.5rem] items-center justify-center px-3 pt-2">
        <img
          src={NyroLogo}
          alt="Nyro"
          className="h-9 w-9 shrink-0 rounded-xl object-contain"
        />
      </div>

      {/* Navigation */}
      <nav className="flex-1 space-y-1 overflow-y-auto px-3 py-2">
        {NAV_ITEMS.map((item, i) => {
          if ("type" in item && item.type === "divider") {
            return (
              <div key={i} className="sidebar-divider my-3 border-t border-slate-200/80" />
            );
          }
          const { label, path, icon: Icon } = item as {
            label: string;
            path: string;
            icon: LucideIcon;
          };
          const displayLabel = isZh ? ({
            Dashboard: "概览", Providers: "提供商", "Available Models": "可用模型",
            "Model Ratings": "模型评分", "Model Mapping": "模型映射", "API Keys": "密钥",
            Connect: "接入", Logs: "日志", Stats: "统计", Performance: "性能", Extensions: "扩展", Settings: "系统设置",
          } as Record<string, string>)[label] ?? label : label;
          return (
            <NavLink
              key={path}
              to={path}
              aria-label={displayLabel}
              title={collapsed ? displayLabel : undefined}
              end={path === "/"}
              className={({ isActive }) =>
                cn(
                  "sidebar-item group flex items-center rounded-xl py-2.5 text-[13px] font-medium transition-all duration-200 cursor-pointer",
                  collapsed
                    ? "mx-auto h-11 w-11 justify-center px-0"
                    : "gap-3 px-3",
                  isActive
                    ? "sidebar-item-active bg-slate-900 text-white shadow-[inset_0_1px_0_rgba(255,255,255,0.12),0_6px_14px_rgba(15,23,42,0.22)]"
                    : "text-slate-600 hover:bg-white/70 hover:text-slate-900"
                )
              }
            >
              <Icon className="h-4 w-4 shrink-0" />
              {!collapsed && <span>{displayLabel}</span>}
            </NavLink>
          );
        })}
      </nav>

      {/* Collapse Toggle */}
      <button
        onClick={onToggle}
        aria-label={collapsed ? (isZh ? "展开导航" : "Expand navigation") : (isZh ? "折叠导航" : "Collapse navigation")}
        className="sidebar-toggle flex h-11 items-center justify-center border-t border-slate-200/80 text-slate-500 transition-colors hover:text-slate-900 cursor-pointer"
      >
        <ChevronLeft
          className={cn(
            "h-4 w-4 transition-transform duration-300",
            collapsed && "rotate-180"
          )}
        />
      </button>
    </aside>
  );
}
