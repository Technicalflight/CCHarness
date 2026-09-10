// Model capability heuristics. Providers don't expose modality metadata, so
// image generation support is inferred from the model name — the list covers
// the common image-capable models seen on OpenAI-compatible relays.

const IMAGE_MODEL_RE =
  /(image|img-|imgen|flux|dall[-·]?e|diffusion|seedream|seededit|cogview|midjourney|mj-?v|ideogram|recraft|kolors|photon|banana|grok-2-image|gpt-image|gemini.*image|sora)/i;

/** True when the model name looks like an image-generation model. */
export function isImageModel(model: string): boolean {
  return IMAGE_MODEL_RE.test(model);
}

/** Chat-mode complement: everything the image heuristic doesn't claim —
 *  used to filter the model picker in 对话 mode. */
export function isChatModel(model: string): boolean {
  return !isImageModel(model);
}
