// Custom dropdown — replaces native <select> everywhere. Auto-flips
// up/down by available space, supports keyboard navigation.
import { useEffect, useRef, useState, type ReactNode } from "react";

export interface DropdownOption {
  value: string;
  label: ReactNode;
}

export function Dropdown({
  value,
  options,
  onChange,
  placeholder = "请选择…",
  minWidth,
  compact,
}: {
  value: string;
  options: DropdownOption[];
  onChange: (value: string) => void;
  placeholder?: string;
  minWidth?: number;
  compact?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [dropUp, setDropUp] = useState(false);
  const [hl, setHl] = useState(0);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [open]);

  useEffect(() => {
    const idx = options.findIndex((o) => o.value === value);
    setHl(idx >= 0 ? idx : 0);
  }, [options, value]);

  const toggle = () => {
    if (!open) {
      const rect = ref.current?.getBoundingClientRect();
      if (rect) {
        const spaceBelow = window.innerHeight - rect.bottom;
        setDropUp(spaceBelow < Math.min(options.length * 34 + 12, 320) && rect.top > spaceBelow);
      }
      const idx = options.findIndex((o) => o.value === value);
      setHl(idx >= 0 ? idx : 0);
    }
    setOpen((o) => !o);
  };

  const pick = (v: string) => {
    onChange(v);
    setOpen(false);
  };

  const current = options.find((o) => o.value === value);

  return (
    <div className={`dropdown ${compact ? "compact" : ""}`} ref={ref} style={minWidth ? { minWidth } : undefined}>
      <button
        type="button"
        className="dropdown-btn"
        onClick={toggle}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            if (!open) {
              toggle();
            } else {
              setHl((h) => (e.key === "ArrowDown" ? Math.min(h + 1, options.length - 1) : Math.max(h - 1, 0)));
            }
          }
          if (e.key === "Enter" && open) {
            e.preventDefault();
            const o = options[hl];
            if (o) pick(o.value);
          }
          if (e.key === "Escape") setOpen(false);
        }}
      >
        <span className={current ? "" : "dropdown-placeholder"}>{current?.label ?? placeholder}</span>
        <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
          <path d="M1 3.5 L5 7.5 L9 3.5" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
        </svg>
      </button>
      {open && (
        <div className={`mp-menu ${dropUp ? "up" : "down"} dropdown-menu`}>
          {options.length === 0 && <div className="dropdown-empty">暂无可选项</div>}
          {options.map((o, i) => (
            <button
              key={o.value}
              className={`mp-item ${o.value === value ? "selected" : ""} ${i === hl ? "hl" : ""}`}
              onMouseEnter={() => setHl(i)}
              onClick={() => pick(o.value)}
            >
              <span>{o.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
