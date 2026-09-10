// AI/LLM brand icons via @lobehub/icons (v1 line — the last release with a
// react >=18 peer; v2+ requires react 19 + antd). `sideEffects: false` keeps
// the bundle tree-shaken to the brands referenced below.
//
// Two resolvers share one rule table:
//   ModelMark  — match by MODEL ID (gpt-4o → OpenAI, glm-4 → GLM, …) so every
//                model chip / picker row carries its own brand mark
//   BrandMark  — match by PROVIDER (name + base_url + model ids); kept for
//                provider cards and provider-only rows
// Brands without a match fall back to the colored dot.
// Source: https://lobehub.com/icons · usage per the lobe-icons skill.

import type { CSSProperties, ComponentType } from "react";
import {
  Baichuan,
  ByteDance,
  Claude,
  DeepSeek,
  Gemini,
  Google,
  Grok,
  Groq,
  Hunyuan,
  Meta,
  Minimax,
  Mistral,
  Moonshot,
  Ollama,
  OpenAI,
  Spark,
  Tongyi,
  Wenxin,
  Yi,
  Zhipu,
} from "@lobehub/icons";
import type { Provider } from "../types";
import { providerColor } from "./color";

type IconComp = ComponentType<{
  size?: number | string;
  className?: string;
  style?: CSSProperties;
}>;

interface BrandRule {
  re: RegExp;
  mono: IconComp;
  title: string;
}

/** One rule table for both resolvers — first match wins. */
const RULES: BrandRule[] = [
  // model-id-first brands
  { re: /claude|anthropic/i, mono: Claude, title: "Claude / Anthropic" },
  { re: /deepseek/i, mono: DeepSeek, title: "DeepSeek" },
  { re: /glm|chatglm|zhipu|bigmodel|清言/i, mono: Zhipu, title: "智谱 GLM" },
  { re: /kimi|moonshot/i, mono: Moonshot, title: "Kimi / Moonshot" },
  { re: /qwen|qwq|qvq|tongyi|wanx|dashscope/i, mono: Tongyi, title: "通义千问" },
  { re: /gemini|gemma/i, mono: Gemini, title: "Gemini" },
  { re: /gpt|chatgpt|o1|o3|o4|davinci|dall|whisper|openai/i, mono: OpenAI, title: "OpenAI" },
  { re: /doubao|bytedance|seed-|skylark/i, mono: ByteDance, title: "豆包 / 字节" },
  { re: /grok|xai/i, mono: Grok, title: "Grok" },
  { re: /minimax|abab/i, mono: Minimax, title: "MiniMax" },
  { re: /hunyuan/i, mono: Hunyuan, title: "腾讯混元" },
  { re: /ernie|wenxin|baidu|qianfan/i, mono: Wenxin, title: "文心一言" },
  { re: /spark|xinghuo|xfyun|iflytek/i, mono: Spark, title: "讯飞星火" },
  { re: /baichuan/i, mono: Baichuan, title: "百川" },
  { re: /\byi[-_ ]|lingyi|01\.ai|01ai/i, mono: Yi, title: "零一万物" },
  { re: /mistral|mixtral|codestral|ministral/i, mono: Mistral, title: "Mistral" },
  { re: /llama(?!va)|meta-|^meta/i, mono: Meta, title: "Llama / Meta" },
  { re: /gemma/i, mono: Google, title: "Gemma" },
  { re: /groq/i, mono: Groq, title: "Groq" },
  { re: /ollama|localhost|127\.0\.0\.1|11434/i, mono: Ollama, title: "Ollama" },
];

function resolveRule(hay: string): BrandRule | null {
  if (!hay) return null;
  const low = hay.toLowerCase();
  return RULES.find((r) => r.re.test(low)) ?? null;
}

/** Prefer the brand-color variant when the icon ships one. */
function colorOf(Icon: unknown): IconComp | undefined {
  const c = (Icon as Record<string, unknown>)?.Color;
  return typeof c === "function" || typeof c === "object" ? (c as IconComp) : undefined;
}

function Dot({ seed, size }: { seed: string; size: number }) {
  return (
    <span
      className="provider-dot"
      style={{
        background: providerColor(seed),
        width: Math.max(8, size * 0.6),
        height: Math.max(8, size * 0.6),
        borderRadius: "50%",
        display: "inline-block",
      }}
    />
  );
}

/**
 * Icon for a MODEL ID — the primary mark everywhere a specific model is
 * named (model picker rows, model chips, lane headers).
 */
export function ModelMark({ model, size = 14 }: { model?: string | null; size?: number }) {
  const hit = resolveRule(model ?? "");
  if (hit) {
    const Comp = colorOf(hit.mono) ?? hit.mono;
    return (
      <span className="lobe-mark" title={hit.title} style={{ width: size, height: size, display: "inline-flex" }}>
        <Comp size={size} style={{ width: size, height: size }} />
      </span>
    );
  }
  return <Dot seed={model ?? "?"} size={size} />;
}

/** Icon for a PROVIDER — matches against name, base_url and its model ids. */
export function BrandMark({ provider, size = 14 }: { provider?: Provider | null; size?: number }) {
  if (!provider) return <Dot seed="?" size={size} />;
  const hay = [provider.name, provider.base_url, ...(provider.models ?? [])].join(" ");
  const hit = resolveRule(hay);
  if (hit) {
    const Comp = colorOf(hit.mono) ?? hit.mono;
    return (
      <span className="lobe-mark" title={hit.title} style={{ width: size, height: size, display: "inline-flex" }}>
        <Comp size={size} style={{ width: size, height: size }} />
      </span>
    );
  }
  return <Dot seed={provider.id} size={size} />;
}
