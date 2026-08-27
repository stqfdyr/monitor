import { Activity, ArrowDownUp, Server, Wallet } from "lucide-react"

import { Card } from "@/components/ui/card"
import { monthUsage } from "@/components/NodeCard"
import type { Node } from "@/lib/api"
import { bytes, rate } from "@/lib/format"

/** Normalises a billing cycle to a monthly figure so prices can be added up. */
const MONTHS: Record<string, number> = {
  monthly: 1, quarterly: 3, semiannual: 6, yearly: 12, biennial: 24, triennial: 36,
}

function Tile({ icon: Icon, label, value, hint }: {
  icon: typeof Server; label: string; value: string; hint?: string
}) {
  return (
    <Card className="gap-0 p-4">
      <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <Icon className="size-3.5" />
        {label}
      </div>
      <div className="tnum mt-1.5 text-2xl font-semibold">{value}</div>
      {hint && <div className="tnum mt-0.5 text-xs text-muted-foreground">{hint}</div>}
    </Card>
  )
}

export function Summary({ nodes }: { nodes: Node[] }) {
  const online = nodes.filter((n) => n.online)
  const live = online.map((n) => n.metrics!).filter(Boolean)

  const down = live.reduce((sum, m) => sum + m.net_rx, 0)
  const up = live.reduce((sum, m) => sum + m.net_tx, 0)
  const month = nodes.reduce((sum, n) => sum + monthUsage(n), 0)
  const total = nodes.reduce((sum, n) => sum + n.total_rx + n.total_tx, 0)

  // Only nodes with a recurring cycle contribute; one-off purchases do not.
  const monthlyCost = nodes.reduce((sum, n) => {
    const months = MONTHS[n.billing_cycle]
    return months && n.price > 0 ? sum + n.price / months : sum
  }, 0)
  const currency = nodes.find((n) => n.price > 0)?.currency ?? "USD"

  const cpu = live.length ? live.reduce((s, m) => s + m.cpu, 0) / live.length : 0

  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      <Tile
        icon={Server}
        label="节点"
        value={`${online.length} / ${nodes.length}`}
        hint={nodes.length - online.length > 0 ? `${nodes.length - online.length} 个离线` : "全部在线"}
      />
      <Tile icon={Activity} label="平均 CPU" value={`${cpu.toFixed(1)}%`} hint={`实时 ↓${rate(down)} ↑${rate(up)}`} />
      <Tile icon={ArrowDownUp} label="本月流量" value={bytes(month)} hint={`累计 ${bytes(total)}`} />
      <Tile
        icon={Wallet}
        label="月均成本"
        value={monthlyCost > 0 ? `${monthlyCost.toFixed(2)} ${currency}` : "—"}
        hint={monthlyCost > 0 ? `约 ${(monthlyCost * 12).toFixed(2)} ${currency}/年` : "未配置价格"}
      />
    </div>
  )
}
