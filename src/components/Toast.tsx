import { useApp } from "../store";
import { Icon } from "../lib/icons";

export function Toasts() {
  const toasts = useApp((s) => s.toasts);
  const dismiss = useApp((s) => s.dismissToast);
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`} onClick={() => dismiss(t.id)}>
          <span className="t-icon">
            <Icon name={t.kind === "error" ? "warning" : t.kind === "success" ? "checkCircle" : "info"} size={15} />
          </span>
          <span>{t.text}</span>
        </div>
      ))}
    </div>
  );
}
