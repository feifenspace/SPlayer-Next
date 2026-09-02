import { toast } from "@/composables/useToast";

/** 使用选区复制，兼容局域网 HTTP 等非安全上下文。 */
const writeClipboardTextWithSelection = (text: string): void => {
  const activeElement =
    document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.readOnly = true;
  textarea.setAttribute("aria-hidden", "true");
  textarea.style.position = "fixed";
  textarea.style.opacity = "0";
  textarea.style.pointerEvents = "none";
  document.body.append(textarea);
  textarea.focus();
  textarea.select();
  textarea.setSelectionRange(0, textarea.value.length);

  const handleCopy = (event: ClipboardEvent): void => {
    if (!event.clipboardData) return;
    event.clipboardData.setData("text/plain", text);
    event.preventDefault();
  };
  document.addEventListener("copy", handleCopy, { once: true });

  try {
    if (typeof document.execCommand !== "function" || !document.execCommand("copy")) {
      throw new Error("浏览器拒绝复制文本");
    }
  } finally {
    document.removeEventListener("copy", handleCopy);
    textarea.remove();
    activeElement?.focus();
  }
};

/** 优先使用 Clipboard API，并在不可用时回退到同步选区复制。 */
export const writeClipboardText = async (text: string): Promise<void> => {
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return;
    } catch {
      // 某些浏览器即使暴露 API 仍会按权限拒绝，继续尝试兼容路径。
    }
  }
  writeClipboardTextWithSelection(text);
};

/**
 * 复制文本到剪贴板，自动 toast 反馈
 */
export const useCopyText = () => {
  const { t } = useI18n();

  /**
   * 复制文本
   * @param text - 要复制的内容
   */
  const copy = async (text: string | null | undefined): Promise<void> => {
    if (!text) {
      toast.error(t("common.copyFailed"));
      return;
    }
    try {
      await writeClipboardText(text);
      toast.success(t("common.copied"));
    } catch {
      toast.error(t("common.copyFailed"));
    }
  };

  return { copy };
};
