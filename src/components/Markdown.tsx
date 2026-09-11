// Markdown rendering with GFM, code-block headers, copy buttons and
// Mermaid diagram rendering (```mermaid blocks render as SVG, with a
// source toggle and graceful fallback when the diagram fails to parse).
import { memo, useEffect, useState } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

// mermaid is heavy (~1MB): loaded on demand — the first ```mermaid block
// in a session triggers the dynamic import, everything else never pays.
let mermaidMod: Promise<{ default: typeof import("mermaid").default }> | null = null;
function loadMermaid() {
  if (!mermaidMod) {
    mermaidMod = import("mermaid");
    mermaidMod.then(({ default: mermaid }) => {
      const light = document.documentElement.getAttribute("data-theme") === "light";
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        theme: light ? "neutral" : "dark",
      });
    });
  }
  return mermaidMod;
}

function MermaidBlock({ code }: { code: string }) {
  const [svg, setSvg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [showSource, setShowSource] = useState(false);
  useEffect(() => {
    let cancelled = false;
    setSvg(null);
    setErr(null);
    loadMermaid()
      .then(({ default: mermaid }) => mermaid.render(`mmd-${Math.random().toString(36).slice(2)}`, code))
      .then(({ svg: out }) => {
        if (!cancelled) setSvg(out);
      })
      .catch((e) => {
        if (!cancelled) setErr(String(e?.message ?? e));
      });
    return () => {
      cancelled = true;
    };
  }, [code]);
  return (
    <div className="code-block mermaid-block">
      <div className="code-head">
        <span>mermaid</span>
        <button onClick={() => setShowSource((s) => !s)}>{showSource ? "图表" : "源码"}</button>
      </div>
      {showSource || err ? (
        <pre>
          <code>{code}</code>
        </pre>
      ) : svg ? (
        <div className="mermaid-view" dangerouslySetInnerHTML={{ __html: svg }} />
      ) : (
        <div className="mermaid-view" style={{ opacity: 0.6, padding: "18px 14px", fontSize: 12 }}>
          正在渲染图表…
        </div>
      )}
      {err && <div className="mermaid-err">图表解析失败：{err.slice(0, 160)}</div>}
    </div>
  );
}

function CodeBlock({ lang, code }: { lang: string; code: string }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
      setTimeout(() => setCopied(false), 1400);
    } catch {
      /* clipboard unavailable */
    }
  };
  return (
    <div className="code-block">
      <div className="code-head">
        <span>{lang || "text"}</span>
        <button onClick={copy}>{copied ? "已复制 ✓" : "复制"}</button>
      </div>
      <pre>
        <code>{code}</code>
      </pre>
    </div>
  );
}

function extractText(node: unknown): string {
  if (node == null) return "";
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(extractText).join("");
  if (typeof node === "object" && "props" in (node as Record<string, unknown>)) {
    const props = (node as { props?: { children?: unknown } }).props;
    return extractText(props?.children);
  }
  return "";
}

function languageOf(className: string | undefined): string {
  if (!className) return "";
  const m = className.match(/language-([\w+-]+)/);
  return m ? m[1] : "";
}

const components: Components = {
  pre({ children }) {
    // react-markdown hands us <pre><code className="language-x">…</code></pre>
    const child = Array.isArray(children) ? children[0] : children;
    let lang = "";
    let code = "";
    if (child && typeof child === "object" && "props" in (child as object)) {
      const props = (child as { props?: { className?: string; children?: unknown } }).props;
      lang = languageOf(props?.className);
      code = extractText(props?.children);
    } else {
      code = extractText(child);
    }
    if (lang === "mermaid") {
      return <MermaidBlock code={code.replace(/\n$/, "")} />;
    }
    return <CodeBlock lang={lang} code={code.replace(/\n$/, "")} />;
  },
  // ExtraProps adds `node?: Element` — destructure it out so it never leaks
  // into the DOM as an unknown attribute
  code({ className, children, node: _node, ...rest }) {
    if (className && className.startsWith("language-")) {
      return (
        <code className={className} {...rest}>
          {children}
        </code>
      );
    }
    return (
      <code className="inline" {...rest}>
        {children}
      </code>
    );
  },
};

export const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
