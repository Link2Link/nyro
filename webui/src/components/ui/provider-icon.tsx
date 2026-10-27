import { cn } from "@/lib/utils";
import { useProviderIconMarkup } from "@/lib/provider-icon";
import { NyroIcon } from "@/components/ui/nyro-icon";

interface ProviderIconProps {
  /** Canonical provider identity (preset key or vendor id); outranks name and host. */
  iconKey?: string;
  name?: string;
  baseUrl?: string;
  size?: number;
  className?: string;
  monochrome?: boolean;
  fill?: boolean;
}

export function ProviderIcon({
  iconKey,
  name,
  baseUrl,
  size = 20,
  className,
  monochrome = false,
  fill = false,
}: ProviderIconProps) {
  const { iconKey: resolvedIconKey, iconMarkup } = useProviderIconMarkup({ iconKey, name, baseUrl }, monochrome);
  const fallback = (name || "?").slice(0, 1).toUpperCase();
  // `nyro.svg` renders blank (artwork outside its viewBox); draw the raster mark instead.
  const innerSize = Math.round(size * (fill ? 1 : 0.78));

  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center justify-center overflow-hidden rounded-md border border-slate-200 bg-white/85 text-[10px] font-semibold text-slate-500",
        className,
      )}
      style={{ width: size, height: size }}
      title={name || "provider"}
    >
      {resolvedIconKey === "nyro" ? (
        <NyroIcon size={innerSize} monochrome={monochrome} title={name || "Nyro"} />
      ) : iconMarkup ? (
        <span
          aria-hidden="true"
          className={cn("provider-icon-markup", fill ? "h-full w-full" : "h-[78%] w-[78%]")}
          dangerouslySetInnerHTML={{ __html: iconMarkup }}
        />
      ) : (
        fallback
      )}
    </span>
  );
}
