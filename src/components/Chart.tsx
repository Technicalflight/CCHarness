// Hand-rolled SVG charts — no chart library dependency.
export interface ChartPoint {
  x: number; // 0..1 normalized position
  y: number; // 0..100 percent
}

export function HitRateChart({
  points,
  epochs,
  height = 190,
}: {
  points: ChartPoint[];
  epochs: number[]; // normalized x positions of epoch boundaries
  height?: number;
}) {
  const W = 760;
  const H = height;
  const padL = 38;
  const padR = 12;
  const padT = 14;
  const padB = 24;
  const iw = W - padL - padR;
  const ih = H - padT - padB;
  const px = (x: number) => padL + x * iw;
  const py = (y: number) => padT + (1 - y / 100) * ih;

  const path = points.map((p, i) => `${i === 0 ? "M" : "L"}${px(p.x).toFixed(1)},${py(p.y).toFixed(1)}`).join(" ");
  const area =
    points.length > 0
      ? `${path} L${px(points[points.length - 1].x).toFixed(1)},${py(0)} L${px(points[0].x).toFixed(1)},${py(0)} Z`
      : "";

  return (
    <svg viewBox={`0 0 ${W} ${H}`} style={{ width: "100%", height: "auto" }} role="img">
      <defs>
        <linearGradient id="hr-fill" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="var(--accent)" stopOpacity="0.28" />
          <stop offset="1" stopColor="var(--accent)" stopOpacity="0.02" />
        </linearGradient>
      </defs>
      {/* grid */}
      {[0, 25, 50, 75, 100].map((v) => (
        <g key={v}>
          <line x1={padL} x2={W - padR} y1={py(v)} y2={py(v)} stroke="var(--border)" strokeWidth="1" />
          <text x={padL - 7} y={py(v) + 3.5} textAnchor="end" fontSize="10" fill="var(--text-faint)" fontFamily="var(--mono)">
            {v}
          </text>
        </g>
      ))}
      {/* 95% target line */}
      <line
        x1={padL}
        x2={W - padR}
        y1={py(95)}
        y2={py(95)}
        stroke="var(--good)"
        strokeWidth="1"
        strokeDasharray="5 4"
        opacity="0.55"
      />
      <text x={W - padR} y={py(95) - 4} textAnchor="end" fontSize="9.5" fill="var(--good)" fontFamily="var(--mono)">
        目标 95%
      </text>
      {/* epoch markers */}
      {epochs.map((x, i) => (
        <g key={i}>
          <line x1={px(x)} x2={px(x)} y1={padT} y2={py(0)} stroke="var(--warn)" strokeWidth="1" strokeDasharray="3 4" opacity="0.5" />
          <text x={px(x) + 3} y={padT + 9} fontSize="9" fill="var(--warn)" fontFamily="var(--mono)">
            E{i + 1}
          </text>
        </g>
      ))}
      {/* series */}
      {points.length > 1 && (
        <>
          <path d={area} fill="url(#hr-fill)" />
          <path d={path} fill="none" stroke="var(--accent)" strokeWidth="2" strokeLinejoin="round" strokeLinecap="round" />
          {points.map((p, i) => (
            <circle key={i} cx={px(p.x)} cy={py(p.y)} r="2.6" fill="var(--accent)" />
          ))}
        </>
      )}
      {points.length === 0 && (
        <text x={W / 2} y={H / 2} textAnchor="middle" fontSize="12" fill="var(--text-faint)">
          暂无请求数据 —— 发送第一条消息后，前缀命中率会出现在这里
        </text>
      )}
    </svg>
  );
}

export function Sparkline({ values, width = 90, height = 22 }: { values: number[]; width?: number; height?: number }) {
  if (values.length < 2) return null;
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const path = values
    .map((v, i) => {
      const x = (i / (values.length - 1)) * (width - 2) + 1;
      const y = height - 2 - ((v - min) / span) * (height - 4);
      return `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg width={width} height={height} style={{ display: "block" }}>
      <path d={path} fill="none" stroke="var(--good)" strokeWidth="1.5" strokeLinejoin="round" />
    </svg>
  );
}
