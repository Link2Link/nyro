import { useMemo, useState } from "react";
import { Check, ChevronsUpDown, Plus } from "lucide-react";

import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";

export type ComboboxOption = {
  value: string;
  label: string;
};

type ComboboxProps = {
  options: ComboboxOption[];
  value?: string;
  placeholder?: string;
  searchPlaceholder?: string;
  emptyText?: string;
  /** Label for the "use typed value" row shown in `allowCustom` mode. */
  customCreateText?: string;
  onValueChange: (value: string) => void;
  className?: string;
  /**
   * Allow committing a typed value that is not among `options` — the list
   * shows a "use '<query>'" row that selects the raw input as-is.
   */
  allowCustom?: boolean;
};

export function Combobox({
  options,
  value,
  placeholder = "Select...",
  searchPlaceholder = "Search...",
  emptyText = "No options",
  customCreateText = "Use",
  onValueChange,
  className,
  allowCustom = false,
}: ComboboxProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const selected = useMemo(
    () => options.find((option) => option.value === value),
    [options, value],
  );

  const trimmedQuery = query.trim();
  const queryIsKnownOption =
    trimmedQuery !== ""
    && options.some(
      (option) => option.value === trimmedQuery || option.label === trimmedQuery,
    );
  const showCreateItem = allowCustom && trimmedQuery !== "" && !queryIsKnownOption;

  return (
    <Popover
      open={open}
      onOpenChange={(nextOpen) => {
        setOpen(nextOpen);
        if (nextOpen) setQuery("");
      }}
    >
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          role="combobox"
          aria-expanded={open}
          className={cn("h-10 w-full justify-between rounded-md px-3 font-normal", className)}
        >
          <span className="truncate text-left">
            {selected?.label ?? (value ? value : placeholder)}
          </span>
          <ChevronsUpDown className="ml-2 h-4 w-4 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="nyro-shadcn-select-content w-[var(--radix-popover-trigger-width)] p-0">
        <Command>
          <CommandInput
            placeholder={searchPlaceholder}
            value={query}
            onValueChange={setQuery}
          />
          <CommandList>
            {showCreateItem ? (
              <CommandGroup>
                <CommandItem
                  value={`__create__:${trimmedQuery}`}
                  onSelect={() => {
                    onValueChange(trimmedQuery);
                    setOpen(false);
                  }}
                >
                  <Plus className="h-4 w-4 shrink-0 text-slate-400" />
                  <span className="truncate">
                    {customCreateText} <span className="font-medium">“{trimmedQuery}”</span>
                  </span>
                </CommandItem>
              </CommandGroup>
            ) : null}
            <CommandEmpty>{emptyText}</CommandEmpty>
            <CommandGroup>
              {options.map((option) => (
                <CommandItem
                  key={option.value}
                  value={option.label}
                  onSelect={() => {
                    onValueChange(option.value);
                    setOpen(false);
                  }}
                >
                  <Check
                    className={cn(
                      "h-4 w-4",
                      value === option.value ? "opacity-100" : "opacity-0",
                    )}
                  />
                  <span className="truncate">{option.label}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
