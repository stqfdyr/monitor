import type { ReactNode } from "react"

import { cn } from "@/lib/utils"

type Props = { label: ReactNode; used: number; total: number; format: (n: number) => string; hint?: string }

/** One labelled usage bar: the row that repeats for CPU, memory, swap and disk. */
export function Meter({ label, used, total, format, hint }: Props) {
  const pct = total > 0 ? Math.min(100, (used / total) * 100) : 0
  const tone = pct >= 90 ? "bg-destructive" : pct >= 70 ? "bg-warn" : "bg-ok"
  return (
    <div>
      <div className="flex items-baseline justify-between gap-2 text-xs">
        <span className="text-muted-foreground">{label}</span>
        <span className="tnum">
          {hint ?? (total > 0 ? `${format(used)} / ${format(total)}` : format(used))}
          <span className="text-muted-foreground ml-1.5">{pct.toFixed(0)}%</span>
        </span>
      </div>
      <div className="mt-1 h-1.5 w-full overflow-hidden rounded-full bg-muted">
        <div className={cn("h-full rounded-full transition-[width] duration-500", tone)} style={{ width: `${pct}%` }} />
      </div>
    </div>
  )
}
