import { bytes, percent } from "@/lib/format"
import { cn } from "@/lib/utils"

type Props = {
  used: number
  limit: number
  size?: number
  label?: string
  className?: string
}

/**
 * Monthly allowance as a ring. This is deliberately separate from lifetime
 * traffic: the ring answers "how much of this month's quota is left", which
 * resets every billing period, while the total never resets.
 */
export function TrafficRing({ used, limit, size = 92, label, className }: Props) {
  const pct = limit > 0 ? percent(used, limit) : 0
  const stroke = 8
  const r = (size - stroke) / 2
  const circumference = 2 * Math.PI * r
  const tone = pct >= 90 ? "text-destructive" : pct >= 70 ? "text-warn" : "text-ok"
  const remaining = Math.max(0, limit - used)

  return (
    <div className={cn("flex items-center gap-3", className)}>
      <div className="relative shrink-0" style={{ width: size, height: size }}>
        <svg width={size} height={size} className="-rotate-90" role="img" aria-label={`已用 ${pct.toFixed(0)}%`}>
          <circle
            cx={size / 2} cy={size / 2} r={r} fill="none" strokeWidth={stroke}
            className="stroke-muted"
          />
          {limit > 0 && (
            <circle
              cx={size / 2} cy={size / 2} r={r} fill="none" strokeWidth={stroke} strokeLinecap="round"
              strokeDasharray={circumference}
              strokeDashoffset={circumference * (1 - pct / 100)}
              className={cn("stroke-current transition-[stroke-dashoffset] duration-700", tone)}
            />
          )}
        </svg>
        <div className="absolute inset-0 grid place-content-center text-center leading-none">
          {limit > 0 ? (
            <>
              <div className={cn("tnum text-lg font-semibold", tone)}>{pct.toFixed(0)}%</div>
              <div className="text-[10px] text-muted-foreground mt-0.5">已用</div>
            </>
          ) : (
            <div className="text-[11px] text-muted-foreground">不限</div>
          )}
        </div>
      </div>
      <div className="min-w-0 text-sm">
        {label && <div className="text-xs text-muted-foreground">{label}</div>}
        <div className="tnum font-medium">{bytes(used)}</div>
        {limit > 0 ? (
          <div className="tnum text-xs text-muted-foreground">
            共 {bytes(limit)} · 剩 {bytes(remaining)}
          </div>
        ) : (
          <div className="text-xs text-muted-foreground">未设置额度</div>
        )}
      </div>
    </div>
  )
}
