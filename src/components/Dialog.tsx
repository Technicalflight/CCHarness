// Self-drawn modal dialogs — the app never calls browser alert/confirm/prompt.
import { useEffect, useRef, useState } from "react";

function Shell({
  title,
  children,
  onClose,
}: {
  title: string;
  children: React.ReactNode;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="dialog-overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="dialog" role="dialog" aria-label={title}>
        <h3>{title}</h3>
        {children}
      </div>
    </div>
  );
}

export function ConfirmDialog({
  title,
  description,
  confirmText = "确认",
  danger,
  onConfirm,
  onCancel,
}: {
  title: string;
  description?: string;
  confirmText?: string;
  danger?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <Shell title={title} onClose={onCancel}>
      {description && <p>{description}</p>}
      <div className="d-actions">
        <button className="btn small" onClick={onCancel}>
          取消
        </button>
        <button
          className={`btn small ${danger ? "danger" : "primary"}`}
          autoFocus
          onClick={onConfirm}
        >
          {confirmText}
        </button>
      </div>
    </Shell>
  );
}

/** Close-button interception: keep running in the tray, or exit for real. */
export function CloseAskDialog({
  onTray,
  onQuit,
  onCancel,
}: {
  onTray: () => void;
  onQuit: () => void;
  onCancel: () => void;
}) {
  return (
    <Shell title="关闭 CCHarness" onClose={onCancel}>
      <p>要退出应用，还是最小化到系统托盘继续运行？</p>
      <div style={{ fontSize: 12.5, color: "var(--text-faint)", marginBottom: 4 }}>
        托盘模式下任务继续执行，左键托盘图标或右键菜单可随时回到主界面。
      </div>
      <div className="d-actions">
        <button className="btn small" onClick={onCancel}>
          取消
        </button>
        <button className="btn small primary" autoFocus onClick={onTray}>
          最小化到托盘
        </button>
        <button className="btn small danger" onClick={onQuit}>
          直接退出
        </button>
      </div>
    </Shell>
  );
}

export function PromptDialog({
  title,
  description,
  placeholder,
  initial = "",
  confirmText = "确定",
  validate,
  onConfirm,
  onCancel,
}: {
  title: string;
  description?: string;
  placeholder?: string;
  initial?: string;
  confirmText?: string;
  validate?: (v: string) => string | null; // error text, null = ok
  onConfirm: (value: string) => void;
  onCancel: () => void;
}) {
  const [value, setValue] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const ref = useRef<HTMLInputElement>(null);

  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);

  const submit = () => {
    const v = value.trim();
    const err = validate?.(v) ?? null;
    if (err) {
      setError(err);
      return;
    }
    onConfirm(v);
  };

  return (
    <Shell title={title} onClose={onCancel}>
      {description && <p>{description}</p>}
      <input
        ref={ref}
        style={{ width: "100%", marginBottom: 8 }}
        value={value}
        placeholder={placeholder}
        spellCheck={false}
        onChange={(e) => {
          setValue(e.target.value);
          setError(null);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            submit();
          }
        }}
      />
      {error && (
        <p style={{ color: "var(--bad)", fontSize: 12.5, marginBottom: 10 }}>{error}</p>
      )}
      <div className="d-actions">
        <button className="btn small" onClick={onCancel}>
          取消
        </button>
        <button className="btn small primary" onClick={submit}>
          {confirmText}
        </button>
      </div>
    </Shell>
  );
}
