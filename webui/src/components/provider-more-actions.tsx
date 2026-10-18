import { useState, type ReactNode } from "react";
import { MoreHorizontal } from "lucide-react";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";

export function ProviderMoreActions({ label, children }: { label: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return <Popover open={open} onOpenChange={setOpen}>
    <PopoverTrigger asChild>
      <button type="button" aria-label={label} title={label} className="rounded-lg p-2 text-slate-400 hover:bg-slate-100 hover:text-slate-600 focus-visible:ring-2 focus-visible:ring-ring">
        <MoreHorizontal className="h-4 w-4" />
      </button>
    </PopoverTrigger>
    <PopoverContent align="end" className="w-44 rounded-xl p-1" onClick={(event) => {
      const button = (event.target as HTMLElement).closest("button");
      if (button && !button.disabled) setOpen(false);
    }}>
      <div className="flex flex-col [&>button]:flex [&>button]:items-center [&>button]:gap-2 [&>button]:text-left [&>button]:text-xs [&>button:last-child]:mt-1 [&>button:last-child]:border-t [&>button:last-child]:border-border [&>button:last-child]:text-red-500">
        {children}
      </div>
    </PopoverContent>
  </Popover>;
}
