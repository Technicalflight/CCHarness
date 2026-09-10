// ComposerPet — a 2D flat-illustration desk-pet cat (inline SVG + CSS
// animations) lying ON TOP of the composer's top edge. Same blind-box
// design language as the reference figure: charcoal ceramic body with a
// soft top-left highlight, gold headphones / inner ears / paw badge, warm
// glowing eyes with halo, cream muzzle & paws, pink blush. CSS keyframes
// drive breathing / blinking / lazy tail sway / occasional head tilt; JS
// lerps the head & eyes toward the mouse; clicking pets it → crescent
// "^ ^" smile eyes, fast tail wag, ear wiggle and floating hearts.
import { useEffect, useRef, useState } from "react";

export function ComposerPet({ fallbackRight = 246 }: { fallbackRight?: number }) {
  const wrapRef = useRef<HTMLDivElement>(null);
  const headG = useRef<SVGGElement>(null);
  const eyesG = useRef<SVGGElement>(null);
  const [happy, setHappy] = useState(false);
  const happyTimer = useRef(0);

  // dodge the mode capsule (对话|生图|隔离): track its real width and pin
  // the cat just past its left edge, so a widening capsule (e.g. the
  // isolation badge) never covers the cat. No capsule (hero page) → stay
  // at fallbackRight, near the composer's right edge.
  useEffect(() => {
    const wrap = wrapRef.current;
    if (!wrap) return;
    const capsule = wrap.parentElement?.querySelector<HTMLElement>(".mode-float");
    if (!capsule) {
      wrap.style.right = `${fallbackRight}px`;
      return;
    }
    const apply = () => {
      wrap.style.right = `${capsule.offsetWidth + 10}px`;
    };
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(capsule);
    return () => ro.disconnect();
  }, [fallbackRight]);

  // gaze-follow: lerp the head (±5px) and eyes (±8px parallax) toward the cursor
  useEffect(() => {
    let tx = 0, ty = 0, cx = 0, cy = 0, raf = 0;
    const onMove = (e: MouseEvent) => {
      tx = (e.clientX / window.innerWidth) * 2 - 1;
      ty = (e.clientY / window.innerHeight) * 2 - 1;
    };
    const tick = () => {
      cx += (tx - cx) * 0.06;
      cy += (ty - cy) * 0.06;
      headG.current?.setAttribute(
        "transform",
        `translate(${(cx * 5).toFixed(2)} ${(cy * 2.5).toFixed(2)})`
      );
      eyesG.current?.setAttribute(
        "transform",
        `translate(${(cx * 8).toFixed(2)} ${(cy * 4).toFixed(2)})`
      );
      raf = requestAnimationFrame(tick);
    };
    window.addEventListener("mousemove", onMove);
    raf = requestAnimationFrame(tick);
    return () => {
      window.removeEventListener("mousemove", onMove);
      cancelAnimationFrame(raf);
    };
  }, []);

  const pet = () => {
    window.clearTimeout(happyTimer.current);
    setHappy(true);
    happyTimer.current = window.setTimeout(() => setHappy(false), 1900);
  };

  return (
    <div ref={wrapRef} className={`pet-wrap ${happy ? "happy" : ""}`} title="趴在输入框上的猫 —— 点它一下试试">
      <svg viewBox="0 0 160 112" onClick={pet} aria-hidden>
        <defs>
          <radialGradient id="petBody" cx="38%" cy="28%" r="80%">
            <stop offset="0%" stopColor="#4a5060" />
            <stop offset="55%" stopColor="#2c303b" />
            <stop offset="100%" stopColor="#20232b" />
          </radialGradient>
          <radialGradient id="petGlow" cx="50%" cy="50%" r="50%">
            <stop offset="0%" stopColor="#ffd873" stopOpacity="0.55" />
            <stop offset="100%" stopColor="#ffd873" stopOpacity="0" />
          </radialGradient>
          <linearGradient id="petGold" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="#eccf92" />
            <stop offset="100%" stopColor="#b98f4e" />
          </linearGradient>
        </defs>

        {/* ground shadow (breathes with the body) */}
        <ellipse className="pet-shadow" cx="80" cy="106" rx="46" ry="5" fill="rgba(0,0,0,0.22)" />

        <g className="pet-breathe">
          {/* tail — lazy sway; gold tip */}
          <g className="pet-tail">
            <circle cx="110" cy="84" r="7" fill="url(#petBody)" />
            <circle cx="116" cy="76" r="6" fill="url(#petBody)" />
            <circle cx="120" cy="68" r="5" fill="url(#petBody)" />
            <circle cx="121" cy="61" r="4.2" fill="url(#petBody)" />
            <circle cx="120" cy="55" r="3.6" fill="url(#petGold)" />
          </g>

          {/* body + belly */}
          <ellipse cx="80" cy="88" rx="42" ry="22" fill="url(#petBody)" />
          <ellipse cx="80" cy="92" rx="24" ry="13" fill="#f2ede4" />
          {/* rear paws */}
          <ellipse cx="52" cy="101" rx="10" ry="6" fill="#f2ede4" />
          <ellipse cx="108" cy="101" rx="10" ry="6" fill="#f2ede4" />

          {/* gold paw badge on the chest */}
          <g>
            <circle cx="80" cy="83" r="6.5" fill="url(#petGold)" />
            <circle cx="80" cy="82" r="1.9" fill="#8a6a35" />
            <circle cx="76.8" cy="85" r="1.1" fill="#8a6a35" />
            <circle cx="80" cy="86" r="1.1" fill="#8a6a35" />
            <circle cx="83.2" cy="85" r="1.1" fill="#8a6a35" />
          </g>

          {/* head group: JS gaze translate → CSS tilt → face */}
          <g ref={headG}>
            <g className="pet-head">
              {/* ears (charcoal shell + gold inner) */}
              <g className="pet-ear pet-ear-l">
                <path d="M54 30 L45 5 L74 17 Z" fill="url(#petBody)" />
                <path d="M56 25 L51 11 L67 18 Z" fill="url(#petGold)" />
              </g>
              <g className="pet-ear pet-ear-r">
                <path d="M106 30 L115 5 L86 17 Z" fill="url(#petBody)" />
                <path d="M104 25 L109 11 L93 18 Z" fill="url(#petGold)" />
              </g>
              {/* head sphere */}
              <circle cx="80" cy="52" r="34" fill="url(#petBody)" />
              {/* ceramic highlight */}
              <ellipse cx="63" cy="30" rx="12" ry="6" fill="#ffffff" opacity="0.14" transform="rotate(-22 63 30)" />
              {/* headphone band + cans */}
              <path d="M48 32 A 34 34 0 0 1 112 32" fill="none" stroke="url(#petGold)" strokeWidth="5" strokeLinecap="round" />
              <ellipse cx="45" cy="54" rx="7.5" ry="12" fill="url(#petGold)" />
              <ellipse cx="115" cy="54" rx="7.5" ry="12" fill="url(#petGold)" />
              <ellipse cx="43.4" cy="50" rx="2.2" ry="4" fill="#ffffff" opacity="0.35" />
              <ellipse cx="113.4" cy="50" rx="2.2" ry="4" fill="#ffffff" opacity="0.35" />

              {/* face: JS parallax group around the eyes */}
              <g ref={eyesG}>
                {/* round glowing eyes (core + halo) */}
                <g className="pet-eye">
                  <circle cx="66" cy="50" r="12" fill="url(#petGlow)" />
                  <circle cx="66" cy="50" r="5.6" fill="#ffd873" />
                  <circle cx="67.8" cy="47.8" r="1.6" fill="#fff8e0" />
                </g>
                <g className="pet-eye">
                  <circle cx="94" cy="50" r="12" fill="url(#petGlow)" />
                  <circle cx="94" cy="50" r="5.6" fill="#ffd873" />
                  <circle cx="95.8" cy="47.8" r="1.6" fill="#fff8e0" />
                </g>
                {/* crescent "^ ^" smiles (visible while happy) */}
                <path
                  className="pet-smile"
                  d="M58 52 Q66 43 74 52"
                  fill="none"
                  stroke="#ffd873"
                  strokeWidth="3.6"
                  strokeLinecap="round"
                />
                <path
                  className="pet-smile"
                  d="M86 52 Q94 43 102 52"
                  fill="none"
                  stroke="#ffd873"
                  strokeWidth="3.6"
                  strokeLinecap="round"
                />
              </g>

              {/* muzzle + nose + blush */}
              <ellipse cx="80" cy="65" rx="13" ry="8.5" fill="#f2ede4" />
              <path d="M76.6 61.6 L83.4 61.6 L80 65.4 Z" fill="url(#petGold)" />
              <ellipse cx="55" cy="61" rx="4.2" ry="2.6" fill="#f0a7b6" opacity="0.5" />
              <ellipse cx="105" cy="61" rx="4.2" ry="2.6" fill="#f0a7b6" opacity="0.5" />
            </g>
          </g>

          {/* front paws draped over the composer edge */}
          <ellipse cx="64" cy="104" rx="9.5" ry="6" fill="#f2ede4" />
          <ellipse cx="96" cy="104" rx="9.5" ry="6" fill="#f2ede4" />
        </g>
      </svg>

      {happy && (
        <>
          <span className="pet-heart" style={{ left: 28, top: 30, animationDelay: "0s" }}>♥</span>
          <span className="pet-heart" style={{ left: 74, top: 10, animationDelay: "0.25s" }}>♥</span>
          <span className="pet-heart" style={{ left: 116, top: 34, animationDelay: "0.5s" }}>♥</span>
        </>
      )}
    </div>
  );
}
