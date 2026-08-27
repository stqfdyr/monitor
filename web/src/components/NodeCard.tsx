import { ArrowDown, ArrowUp, Cpu, HardDrive, MemoryStick } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { Card } from "@/components/ui/card"
import { Meter } from "@/components/Meter"
import { TrafficRing } from "@/components/TrafficRing"
import type { Node } from "@/lib/api"
import { bytes, CYCLES, daysUntil, money, rate, uptime } from "@/lib/format"
import { cn } from "@/lib/utils"

/** Which direction the plan meters, matching the node's traffic_mode. */
export function monthUsage(node: Node): number {
  const { month_rx: rx, month_tx: tx } = node
  switch (node.traffic_mode) {
    case "up":
      return tx
    case "down":
      return rx
    case "max":
      return Math.max(rx, tx)
    default:
      return rx + tx
  }
}

export const TRAFFIC_MODES: Record<string, string> = {
  sum: "上下行相加",
  max: "取较大值",
  up: "仅上行",
  down: "仅下行",
}

function Label({ icon: Icon, children }: { icon: typeof Cpu; children: string }) {
  return (
    <span className="inline-flex items-center gap-1">
      <Icon className="size-3" />
      {children}
    </span>
  )
}

function Expiry({ node }: { node: Node }) {
  const days = daysUntil(node.expires_at)
  if (days === null) return null
  const tone = days < 0 ? "text-destructive" : days <= 7 ? "text-warn" : "text-muted-foreground"
  return (
    <span className={tone}>
      {days < 0 ? `已过期 ${-days} 天` : `${days} 天后到期`}
    </span>
  )
}

export function NodeCard({ node, onOpen }: { node: Node; onOpen: () => void }) {
  const m = node.metrics

  return (
    <Card
      onClick={onOpen}
      className="cursor-pointer gap-0 p-5 transition-colors hover:border-ring"
      role="button"
      tabIndex={0}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), onOpen())}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span
              className={cn("size-2 shrink-0 rounded-full", node.online ? "bg-ok" : "bg-muted-foreground/40")}
              title={node.online ? "在线" : "离线"}
            />
            <h3 className="truncate font-medium">{node.name}</h3>
          </div>
          <p className="mt-1 truncate text-xs text-muted-foreground">
            {node.os || "等待上报"}
            {node.virt && node.virt !== "none" ? ` · ${node.virt}` : ""}
            {node.arch ? ` · ${node.arch}` : ""}
          </p>
        </div>
        {node.price > 0 && (
          <Badge variant="secondary" className="shrink-0 tnum font-normal">
            {money(node.price, node.currency)}/{CYCLES[node.billing_cycle] ?? node.billing_cycle}
          </Badge>
        )}
      </div>

      {m ? (
        <>
          <div className="mt-4 space-y-2.5">
            <Meter
              label={<Label icon={Cpu}>CPU</Label>}
              used={m.cpu}
              total={100}
              format={(n) => `${n.toFixed(1)}%`}
              hint={`${m.cpu.toFixed(1)}%`}
            />
            <Meter label={<Label icon={MemoryStick}>内存</Label>} used={m.mem_used} total={m.mem_total} format={bytes} />
            {m.swap_total > 0 && <Meter label="Swap" used={m.swap_used} total={m.swap_total} format={bytes} />}
            <Meter label={<Label icon={HardDrive}>硬盘</Label>} used={m.disk_used} total={m.disk_total} format={bytes} />
          </div>

          <div className="mt-4 grid grid-cols-2 gap-x-4 gap-y-1.5 text-xs">
            <span className="tnum inline-flex items-center gap-1 text-muted-foreground">
              <ArrowDown className="size-3 text-ok" />
              {rate(m.net_rx)}
            </span>
            <span className="tnum inline-flex items-center gap-1 text-muted-foreground">
              <ArrowUp className="size-3 text-chart-1" />
              {rate(m.net_tx)}
            </span>
            <span className="text-muted-foreground">运行 {uptime(m.uptime)}</span>
            <span className="tnum text-muted-foreground">负载 {m.load[0].toFixed(2)}</span>
          </div>
        </>
      ) : (
        <p className="mt-6 mb-6 text-center text-sm text-muted-foreground">
          {node.last_seen ? "已离线" : "等待 agent 首次连接"}
        </p>
      )}

      <div className="mt-4 border-t pt-4">
        <TrafficRing used={monthUsage(node)} limit={node.traffic_limit} size={76} label="本月流量" />
        <div className="mt-3 flex items-center justify-between text-xs text-muted-foreground">
          <span className="tnum">
            累计 ↓{bytes(node.total_rx)} ↑{bytes(node.total_tx)}
          </span>
          <Expiry node={node} />
        </div>
      </div>
    </Card>
  )
}
