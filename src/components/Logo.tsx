// Brand mark: two interlocking chevrons forming a "harness" bit.
export function Logo({ size = 30 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 48 48" fill="none" aria-hidden>
      <defs>
        <linearGradient id="cc-g" x1="0" y1="0" x2="48" y2="48">
          <stop offset="0" stopColor="#5c9bff" />
          <stop offset="1" stopColor="#a78bfa" />
        </linearGradient>
      </defs>
      <rect x="2" y="2" width="44" height="44" rx="11" fill="var(--bg2)" stroke="var(--border-strong)" />
      <path
        d="M13 15 L23 24 L13 33"
        stroke="url(#cc-g)"
        strokeWidth="4.5"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M25 15 L35 24 L25 33"
        stroke="url(#cc-g)"
        strokeWidth="4.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        opacity="0.45"
      />
    </svg>
  );
}
