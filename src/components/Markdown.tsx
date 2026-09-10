// Markdown rendering with GFM, code-block headers and copy buttons.
import { memo, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { ComponentPropsWithoutRef } from "react";

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

const components = {
  pre({ children }: ComponentPropsWithoutRef<"pre">) {
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
    return <CodeBlock lang={lang} code={code.replace(/\n$/, "")} />;
  },
  code({ className, children, ...rest }: ComponentPropsWithoutRef<"code">) {
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
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components as never}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
