import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/// Copy text to the clipboard, working in insecure contexts.
///
/// `navigator.clipboard` only exists in secure contexts (HTTPS or
/// localhost); the console is routinely served over plain HTTP on the LAN
/// (e.g. http://192.168.31.2:19531), where `navigator.clipboard` is
/// `undefined`. Fall back to the legacy `execCommand("copy")` path with a
/// transient off-screen textarea so copy buttons keep working everywhere.
export async function copyToClipboard(text: string): Promise<boolean> {
  if (typeof navigator !== "undefined" && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to the legacy path
    }
  }

  try {
    const textarea = document.createElement("textarea");
    textarea.value = text;
    // Position off-screen and make it inert to focus/scroll side effects.
    textarea.style.position = "fixed";
    textarea.style.top = "-1000px";
    textarea.style.opacity = "0";
    textarea.setAttribute("readonly", "");
    document.body.appendChild(textarea);
    const selection = document.getSelection();
    const selectedRange = selection && selection.rangeCount > 0
      ? selection.getRangeAt(0)
      : null;
    textarea.select();
    textarea.setSelectionRange(0, text.length);
    const ok = document.execCommand("copy");
    document.body.removeChild(textarea);
    if (selectedRange && selection) {
      selection.removeAllRanges();
      selection.addRange(selectedRange);
    }
    return ok;
  } catch {
    return false;
  }
}
