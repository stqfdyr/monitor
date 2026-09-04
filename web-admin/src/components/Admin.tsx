import { useEffect, useRef, useState } from "react"
import { flushSync } from "react-dom"
import { CalendarClock, Copy, Database, Download, GripVertical, Palette, Pencil, Plus, Radio, RefreshCw, Server, Settings, Shield, Trash2, Upload } from "lucide-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, changes, provisioningSite, upload, type Node, type PingTask } from "@/lib/api"
import { bytes, CYCLES, FOREVER, money, monthUsage, uptime } from "@/lib/format"

const GIB = 1024 ** 3
// Counters the panel can correct after migration or an accounting error.
const TRAFFIC_FIELDS = [
  ["total_rx", "累计下行"],
  ["total_tx", "累计上行"],
  ["month_rx", "本月下行"],
  ["month_tx", "本月上行"],
] as const
const TRAFFIC_MODES: Record<string, string> = {
  sum: "上下行相加",
  max: "取较大值",
  up: "仅上行",
  down: "仅下行",
}

// Reordering rides on the browser's view transitions, so rows that make way
// slide. Browsers without it jump.
function animate(update: () => void) {
  if (document.startViewTransition) document.startViewTransition(() => flushSync(update))
  else update()
}

function copy(text: string) {
  navigator.clipboard.writeText(text).then(
    () => toast.success("已复制"),
    () => toast.error("复制失败"),
  )
}

// Every address a node has, each click-to-copy: pasting one into an ssh
// command is the reason it is shown at all.
function Addresses({ node }: { node: Node }) {
  const reported = [node.ipv4, node.ipv6].filter(Boolean) as string[]
  // `ip` is only where the agent's connection came from: the fallback for an
  // agent too old to report its own interfaces.
  const list = reported.length ? reported : ([node.ip].filter(Boolean) as string[])
  if (!list.length) return <span className="text-sm text-muted-foreground">—</span>
  return (
    <div className="flex flex-col items-start gap-y-0.5">
      {list.map((address) => (
        <button
          key={address}
          type="button"
          onClick={() => copy(address)}
          title="点击复制"
          className="tnum group inline-flex items-center gap-1 text-sm hover:text-foreground"
        >
          {address}
          <Copy className="size-3 shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
        </button>
      ))}
    </div>
  )
}

function Field({ label, hint, className = "", children }: { label: string; hint?: string; className?: string; children: React.ReactNode }) {
  return (
    <div className={`space-y-2 ${className}`}>
      <Label className="text-sm font-medium">{label}</Label>
      {children}
      {hint && <p className="text-xs leading-relaxed text-muted-foreground">{hint}</p>}
    </div>
  )
}


function ConfirmDialog({ title, description, confirmLabel, busy = false, onClose, onConfirm }: {
  title: string
  description: string
  confirmLabel: string
  busy?: boolean
  onClose: () => void
  onConfirm: () => void
}) {
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription className="leading-relaxed">{description}</DialogDescription>
        </DialogHeader>
        <DialogFooter className="border-t pt-4">
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button variant="destructive" onClick={onConfirm} disabled={busy}>{confirmLabel}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function CreateNode({ onClose, onSaved }: {
  onClose: () => void
  onSaved: () => void
}) {
  const [name, setName] = useState("")
  const [saving, setSaving] = useState(false)

  async function save(e: React.FormEvent) {
    e.preventDefault()
    if (!name.trim()) return toast.error("请填写节点名称")
    setSaving(true)
    try {
      await api("/nodes", {
        method: "POST",
        body: JSON.stringify({ name: name.trim() }),
      })
      toast.success("节点已添加")
      onClose()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>添加节点</DialogTitle>
        </DialogHeader>
        <form className="space-y-4" onSubmit={save}>
          <Field label="名称">
            <Input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="香港 · 甲商家" />
          </Field>
          <DialogFooter className="border-t pt-4">
            <Button type="button" variant="ghost" onClick={onClose}>取消</Button>
            <Button type="submit" disabled={saving}>添加</Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function NodeForm({ node, onClose, onSaved }: {
  node: Node
  onClose: () => void
  onSaved: () => void
}) {
  const [form, setForm] = useState(node)
  const [limitGib, setLimitGib] = useState(String(node.traffic_limit / GIB || ""))
  const [saving, setSaving] = useState(false)
  const gib = (bytes: number) => String(Number((bytes / GIB).toFixed(3)))
  const [traffic, setTraffic] = useState(() =>
    Object.fromEntries(TRAFFIC_FIELDS.map(([k]) => [k, gib(node[k])])) as Record<string, string>,
  )
  // Compared as typed, not as bytes: rounding to GB reads as an edit and would
  // zero out a node that has moved a few MB.
  const pristine = useRef(traffic)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))

  async function save() {
    if (!form.name.trim()) return toast.error("请填写节点名称")
    const patch = changes(node, {
      name: form.name.trim(),
      public: form.public,
      remark: form.remark,
      traffic_mode: form.traffic_mode,
      traffic_limit: Math.round(Number(limitGib) * GIB),
      traffic_reset_day: Math.min(31, Math.max(1, Math.round(Number(form.traffic_reset_day) || 1))),
    })
    const correction = Object.fromEntries(
      Object.entries(changes(pristine.current, traffic)).map(([key, value]) => [key, Math.round(Number(value) * GIB)]),
    )
    if ([patch.traffic_limit, ...Object.values(correction)].some((v) => v !== undefined && (!Number.isSafeInteger(v) || v < 0))) {
      return toast.error("流量必须是有效的非负数，且不能超出精确计数范围")
    }
    setSaving(true)
    try {
      // The correction belongs to the new reset period, so save its day first.
      if (Object.keys(patch).length) {
        await api(`/nodes/${node.id}`, { method: "PUT", body: JSON.stringify(patch) })
      }
      if (Object.keys(correction).length) {
        await api(`/nodes/${node.id}/traffic`, {
          method: "PUT",
          body: JSON.stringify(correction),
        })
      }
      toast.success("节点设置已保存")
      onClose()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <Field label="名称">
            <Input value={form.name} onChange={(e) => set("name", e.target.value)} />
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="每月流量额度 (GB)" hint="留空或 0 不限">
              <Input type="number" value={limitGib} onChange={(e) => setLimitGib(e.target.value)} placeholder="1024" />
            </Field>
            <Field label="流量计算方式">
              <Select value={form.traffic_mode} onValueChange={(v) => set("traffic_mode", v)}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {Object.entries(TRAFFIC_MODES).map(([k, v]) => (
                    <SelectItem key={k} value={k}>{v}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="每月重置日" hint="1–31。本月流量按新周期重算，总流量不变">
              <Input type="number" min={1} max={31} value={form.traffic_reset_day} onChange={(e) => set("traffic_reset_day", Number(e.target.value))} />
            </Field>
            <Field label="备注" hint="仅管理员可见">
              <Input value={form.remark ?? ""} onChange={(e) => set("remark", e.target.value)} placeholder="商家、用途" />
            </Field>
          </div>
          <details className="rounded-lg border bg-muted/30 px-3 py-2.5">
            <summary className="cursor-pointer text-sm font-medium">流量校正</summary>
            <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
              按 GB 填入需要校正的值，未修改的计数器继续正常累计。
            </p>
            <div className="mt-3 grid gap-4 sm:grid-cols-2">
              {TRAFFIC_FIELDS.map(([key, label]) => (
                <Field key={key} label={`${label} (GB)`}>
                  <Input
                    type="number"
                    step="0.001"
                    value={traffic[key]}
                    onChange={(e) => setTraffic((t) => ({ ...t, [key]: e.target.value }))}
                  />
                </Field>
              ))}
            </div>
          </details>
          <label className="flex cursor-pointer items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
            <span>
              <span className="block font-medium">公开显示</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">关闭后只在管理后台可见</span>
            </span>
            <Switch checked={form.public} onCheckedChange={(v) => set("public", v)} />
          </label>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button onClick={save} disabled={saving}>保存</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function BillingForm({ node, onClose, onSaved }: {
  node: Node
  onClose: () => void
  onSaved: () => void
}) {
  const [form, setForm] = useState(node)
  // Text, not a number: a numeric state cannot hold "empty", so clearing the
  // box snaps back to 0 mid-typing. Empty means free.
  const [price, setPrice] = useState(node.price > 0 ? String(node.price) : "")
  const [saving, setSaving] = useState(false)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))

  async function save() {
    setSaving(true)
    try {
      await api(`/nodes/${node.id}`, {
        method: "PUT",
        body: JSON.stringify(changes(node, {
          price: Math.max(0, Number(price) || 0),
          currency: form.currency,
          billing_cycle: form.billing_cycle,
          expires_at: form.expires_at || null,
        })),
      })
      toast.success("续费设置已保存")
      onClose()
      onSaved()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="价格" hint="留空或 0 为免费">
              <Input
                type="number"
                min="0"
                step="0.01"
                value={price}
                onChange={(e) => setPrice(e.target.value)}
                placeholder="免费"
              />
            </Field>
            <Field label="货币">
              <Select value={form.currency} onValueChange={(v) => set("currency", v)}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {["USD", "CNY", "EUR", "GBP", "JPY"].map((c) => (
                    <SelectItem key={c} value={c}>{c}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
          </div>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="付款周期">
              <Select value={form.billing_cycle} onValueChange={(v) => set("billing_cycle", v)}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {Object.entries(CYCLES).map(([k, v]) => (
                    <SelectItem key={k} value={k}>{v}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </Field>
            <Field label="到期时间">
              <Input type="date" value={form.expires_at ?? ""} onChange={(e) => set("expires_at", e.target.value)} />
            </Field>
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button onClick={save} disabled={saving}>保存</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

// Built here rather than fetched: the node list already carries the token, so
// looking at an install command is a read, not an act. Reissuing one to show
// it knocks the running agent offline.
function installCommand(site: string, token: string, seconds: number) {
  site = provisioningSite(site)
  if (!site) return ""
  const args = [`--server ${site}`, `--token ${token}`, `--interval ${seconds}`]
  return `curl -fsSL ${site}/install.sh | sh -s -- ${args.join(" ")}`
}

// One command for a batch of machines. The key is the hub's, good only inside
// the window it opened, and each machine trades it for a token of its own --
// so unlike an install command, this text is nobody's credential and can go
// straight into a loop.
function registerCommand(site: string, key: string) {
  site = provisioningSite(site)
  if (!site) return ""
  const args = [`--server ${site}`, `--register ${key}`]
  return `curl -fsSL ${site}/install.sh | sh -s -- ${args.join(" ")}`
}

// The window lives on the hub; this only reads it back and counts down, which
// is also what makes an expired one disappear from the panel without anyone
// clicking anything.
function useRegisterWindow() {
  const [key, setKey] = useState("")
  const [until, setUntil] = useState(0)
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))

  useEffect(() => {
    api<Settings>("/settings")
      .then((s) => { setKey(String(s.register_key ?? "")); setUntil(Number(s.register_until ?? 0)) })
      .catch(() => {})
    const timer = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000)
    return () => clearInterval(timer)
  }, [])

  return {
    key,
    left: key === "" ? 0 : Math.max(0, until - now),
    async open() {
      try {
        const w = await api<{ register_key: string; register_until: string }>("/register-window", { method: "POST" })
        setKey(w.register_key)
        setUntil(Number(w.register_until))
      } catch (e) {
        toast.error((e as Error).message)
      }
    },
    async close() {
      try {
        await api("/register-window", { method: "DELETE" })
        setKey("")
        setUntil(0)
        toast.success("注册窗口已关闭")
      } catch (e) {
        toast.error((e as Error).message)
      }
    },
  }
}

function RegisterDialog({ site, reg, onClose }: {
  site: string
  reg: ReturnType<typeof useRegisterWindow>
  onClose: () => void
}) {
  const command = reg.left > 0 ? registerCommand(site, reg.key) : ""
  const clock = `${Math.floor(reg.left / 60)}:${String(reg.left % 60).padStart(2, "0")}`

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>批量添加</DialogTitle>
        </DialogHeader>
        <div className="space-y-4">
          <p className="text-sm text-muted-foreground">
            开一个一小时的注册窗口。期间这条命令在任意机器上跑一次，那台机器就会自己出现在
            列表里，名字取自它的 hostname。命令里没有任何一台机器的凭证，可以直接进循环。
          </p>
          {command ? (
            <div className="space-y-2">
              <Label className="text-sm font-medium">安装命令</Label>
              <pre className="h-24 overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
                {command}
              </pre>
              <div className="flex items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
                <span>
                  <span className="block font-medium">窗口 {clock} 后自动关闭</span>
                  <span className="mt-0.5 block text-xs text-muted-foreground">
                    到点自动失效，装完了也可以现在就关
                  </span>
                </span>
                <Button variant="outline" size="sm" onClick={reg.close}>立即关闭</Button>
              </div>
            </div>
          ) : (
            <Button onClick={reg.open}>开启一小时窗口</Button>
          )}
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>关闭</Button>
          <Button onClick={() => copy(command)} disabled={!command}>
            <Copy className="size-4" /> 复制
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function InstallDialog({ node, site, onClose, onRotated }: {
  node: Node
  site: string
  onClose: () => void
  onRotated: () => void
}) {
  const [token, setToken] = useState(node.token ?? "")
  const [interval, setInterval] = useState("1")
  const [rotating, setRotating] = useState(false)
  const [confirmRotate, setConfirmRotate] = useState(false)

  const seconds = Math.min(3600, Math.max(1, Math.round(Number(interval) || 1)))
  const command = token ? installCommand(site, token, seconds) : ""

  async function rotate() {
    setRotating(true)
    try {
      const fresh = await api<{ token: string }>(`/nodes/${node.id}/token`, { method: "POST" })
      setToken(fresh.token)
      setConfirmRotate(false)
      toast.success("凭证已换发，需用新命令重装")
      onRotated()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setRotating(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <Field label="上报间隔（秒）" hint="1–3600，默认 1 秒">
            <Input type="number" min={1} max={3600} value={interval} onChange={(e) => setInterval(e.target.value)} />
          </Field>
          <div className="space-y-2">
            <Label className="text-sm font-medium">安装命令</Label>
            <pre className="h-28 overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
              {/* A node added before the hub kept tokens has nothing to show
                  until one is reissued. */}
              {command || "旧版本创建的凭证不可读取，换发后显示"}
            </pre>
          </div>
          <div className="flex items-center justify-between gap-4 rounded-lg border bg-muted/30 px-3 py-2.5 text-sm">
            <span>
              <span className="block font-medium">换发凭证</span>
              <span className="mt-0.5 block text-xs text-muted-foreground">
                旧凭证立即作废，agent 掉线，需用新命令重装
              </span>
            </span>
            <Button variant="outline" size="sm" disabled={rotating} onClick={() => setConfirmRotate(true)}>
              换发
            </Button>
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>关闭</Button>
          <Button onClick={() => copy(command)} disabled={!command}>
            <Copy className="size-4" /> 复制
          </Button>
        </DialogFooter>
      </DialogContent>
      {confirmRotate && (
        <ConfirmDialog
          title={`给「${node.name}」换发凭证？`}
          description="旧凭证立即作废，agent 掉线，必须用新命令重装。仅在凭证可能泄露时使用。"
          confirmLabel="换发凭证"
          busy={rotating}
          onClose={() => setConfirmRotate(false)}
          onConfirm={rotate}
        />
      )}
    </Dialog>
  )
}

function Nodes({ nodes, refresh, site, canProvision }: { nodes: Node[]; refresh: () => void; site: string; canProvision: boolean }) {
  const [creating, setCreating] = useState(false)
  const [editing, setEditing] = useState<Node | null>(null)
  const [billing, setBilling] = useState<Node | null>(null)
  const [installing, setInstalling] = useState<Node | null>(null)
  const [registering, setRegistering] = useState(false)
  const reg = useRegisterWindow()
  const [deleting, setDeleting] = useState<Node | null>(null)
  const [removing, setRemoving] = useState(false)
  const [manualOrder, setManualOrder] = useState<number[]>([])
  const [dragging, setDragging] = useState<number | null>(null)
  const orderBeforeDrag = useRef<number[]>([])
  const byId = new Map(nodes.map((node) => [node.id, node]))
  const orderedIds = new Set(manualOrder)
  const order = [
    ...manualOrder.map((id) => byId.get(id)).filter((node): node is Node => Boolean(node)),
    ...nodes.filter((node) => !orderedIds.has(node.id)),
  ]

  async function remove() {
    if (!deleting) return
    setRemoving(true)
    try {
      await api(`/nodes/${deleting.id}`, { method: "DELETE" })
      toast.success("已删除")
      setDeleting(null)
      refresh()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setRemoving(false)
    }
  }

  // Rows make way while the pointer is down; the order is saved on drop.
  function move(from: number, to: number) {
    if (from < 0 || to < 0 || to >= order.length || from === to) return
    const next = [...order]
    next.splice(to, 0, ...next.splice(from, 1))
    const ids = next.map((node) => node.id)
    animate(() => setManualOrder(ids))
    return ids
  }

  // Dropped outside the table, or cancelled with Escape: back where it was.
  function cancel() {
    setDragging(null)
    const rollback = orderBeforeDrag.current
    if (rollback.length) animate(() => setManualOrder(rollback))
  }

  function save(ids: number[]) {
    setDragging(null)
    const rollback = orderBeforeDrag.current
    if (!rollback.length || ids.join() === rollback.join()) return
    orderBeforeDrag.current = ids
    api("/nodes/order", { method: "PUT", body: JSON.stringify({ ids }) }).then(refresh, (e: Error) => {
      setManualOrder(rollback)
      toast.error(e.message)
    })
  }

  return (
    <div className="space-y-4">
      {!canProvision && <p className="text-sm text-muted-foreground">请通过 HTTPS 域名访问面板后添加或安装节点。</p>}
      <div className="flex justify-end gap-2">
        {/* An open window is visible from the list itself, so nobody has to
            remember they left one open. */}
        <Button variant="outline" disabled={!canProvision} onClick={() => setRegistering(true)}>
          <Server /> 批量添加{reg.left > 0 && ` · ${Math.ceil(reg.left / 60)} 分`}
        </Button>
        <Button disabled={!canProvision} onClick={() => setCreating(true)}>
          <Plus /> 添加节点
        </Button>
      </div>

      <Card className="overflow-x-auto p-0">
        <Table>
          <TableHeader>
            {/* Percentages, or the address column swallows every spare pixel
                and pushes status across the table. */}
            <TableRow>
              <TableHead className="w-[20%]">名称</TableHead>
              <TableHead className="w-[22%]">IP</TableHead>
              <TableHead className="w-[12%]">状态</TableHead>
              <TableHead className="w-[16%]">流量</TableHead>
              <TableHead className="w-[10%]">价格</TableHead>
              <TableHead className="w-[12%]">到期</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {order.map((n, index) => (
              <TableRow
                key={n.id}
                style={{ viewTransitionName: `node-${n.id}` }}
                data-dragging={dragging === n.id || undefined}
                className="transition-opacity data-[dragging]:opacity-40"
                onDragOver={(e) => { e.preventDefault(); e.dataTransfer.dropEffect = "move" }}
                onDragEnter={() => dragging !== null && move(order.findIndex((node) => node.id === dragging), index)}
                onDrop={(e) => { e.preventDefault(); save(order.map((node) => node.id)) }}
              >
                <TableCell>
                  <div className="flex items-center gap-2">
                    <button
                      type="button"
                      draggable
                      className="cursor-grab touch-none rounded p-1 text-muted-foreground hover:bg-muted hover:text-foreground active:cursor-grabbing"
                      title="拖动排序"
                      aria-label={`拖动 ${n.name} 排序`}
                      onDragStart={(e) => {
                        orderBeforeDrag.current = order.map((node) => node.id)
                        setDragging(n.id)
                        e.dataTransfer.effectAllowed = "move"
                        // Firefox refuses to start a drag without payload.
                        e.dataTransfer.setData("text/plain", String(n.id))
                      }}
                      onDragEnd={(e) => (e.dataTransfer.dropEffect === "none" ? cancel() : save(order.map((node) => node.id)))}
                      onKeyDown={(e) => {
                        const delta = e.key === "ArrowUp" ? -1 : e.key === "ArrowDown" ? 1 : 0
                        if (!delta) return
                        e.preventDefault()
                        orderBeforeDrag.current = order.map((node) => node.id)
                        const ids = move(index, index + delta)
                        if (ids) save(ids)
                      }}
                    >
                      <GripVertical className="size-4" />
                    </button>
                    <div className="min-w-0 font-medium">{n.name}</div>
                    {n.country && (
                      <Badge variant="outline" className="shrink-0 font-normal text-muted-foreground">
                        {n.country}
                      </Badge>
                    )}
                  </div>
                </TableCell>
                {/* Addresses live only here, never on the public page. */}
                <TableCell>
                  <Addresses node={n} />
                </TableCell>
                <TableCell>
                  <Badge variant={n.online ? "default" : "secondary"} className="font-normal">
                    {n.online ? "在线" : "离线"}
                  </Badge>
                  {!n.public && <Badge variant="outline" className="ml-1 font-normal">不公开</Badge>}
                  {/* Under the badge, not inside it: the column is a tenth of
                      the table and the three do not share one line. */}
                  {!n.online && n.last_seen > 0 && Date.now() / 1000 - n.last_seen >= 60 && (
                    <div className="tnum mt-1 text-xs text-muted-foreground">
                      {uptime(Date.now() / 1000 - n.last_seen)}
                    </div>
                  )}
                </TableCell>
                {/* Counted by the node's own billing rule, as on the public
                    page. */}
                <TableCell className="tnum text-sm">
                  {bytes(monthUsage(n))}
                  <span className="text-muted-foreground">
                    {" / "}{n.traffic_limit > 0 ? bytes(n.traffic_limit) : FOREVER}
                  </span>
                </TableCell>
                <TableCell className="tnum text-sm">
                  {n.price > 0 ? money(n.price, n.currency) : "免费"}
                </TableCell>
                <TableCell className="text-sm">{n.expires_at || FOREVER}</TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  <Button variant="ghost" size="icon" disabled={!canProvision} onClick={() => setInstalling(n)} title="安装 Agent" aria-label="安装 Agent">
                    <Download />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setEditing(n)} title="编辑节点" aria-label="编辑节点">
                    <Pencil />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setBilling(n)} title="续费设置" aria-label="续费设置">
                    <CalendarClock />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setDeleting(n)} title="删除节点" aria-label="删除节点">
                    <Trash2 className="text-destructive" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
            {nodes.length === 0 && (
              <TableRow>
                <TableCell colSpan={7} className="py-10 text-center text-sm text-muted-foreground">
                  还没有节点，右上角添加
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      {creating && (
        <CreateNode
          onClose={() => setCreating(false)}
          onSaved={refresh}
        />
      )}
      {editing && (
        <NodeForm
          node={editing}
          onClose={() => setEditing(null)}
          onSaved={refresh}
        />
      )}
      {billing && (
        <BillingForm node={billing} onClose={() => setBilling(null)} onSaved={refresh} />
      )}
      {registering && <RegisterDialog site={site} reg={reg} onClose={() => { setRegistering(false); refresh() }} />}

      {installing && (
        <InstallDialog
          node={installing}
          site={site}
          onClose={() => setInstalling(null)}
          onRotated={refresh}
        />
      )}
      {deleting && (
        <ConfirmDialog
          title={`删除节点「${deleting.name}」？`}
          description="历史指标、流量记录和凭证一并删除，不可恢复。"
          confirmLabel="删除节点"
          busy={removing}
          onClose={() => setDeleting(null)}
          onConfirm={remove}
        />
      )}
    </div>
  )
}

function Ping({ nodes }: { nodes: Node[] }) {
  const [tasks, setTasks] = useState<PingTask[]>([])
  const [editing, setEditing] = useState<Partial<PingTask> | null>(null)
  const [deleting, setDeleting] = useState<PingTask | null>(null)
  const [saving, setSaving] = useState(false)
  const [removing, setRemoving] = useState(false)

  const load = () => api<{ tasks: PingTask[] }>("/ping-tasks").then((d) => setTasks(d.tasks)).catch(() => {})
  useEffect(() => { load() }, [])

  async function save() {
    if (!editing) return
    if (!editing.name?.trim() || !editing.target?.trim()) return toast.error("请填写名称和目标")
    setSaving(true)
    try {
      await api("/ping-tasks", { method: "POST", body: JSON.stringify(editing) })
      toast.success("已保存，正在下发")
      setEditing(null)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  async function remove() {
    if (!deleting) return
    setRemoving(true)
    try {
      await api(`/ping-tasks/${deleting.id}`, { method: "DELETE" })
      toast.success("监控已删除")
      setDeleting(null)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setRemoving(false)
    }
  }

  const toggle = (id: number) =>
    setEditing((t) => {
      if (!t) return t
      const nodes = t.nodes ?? []
      return { ...t, nodes: nodes.includes(id) ? nodes.filter((n) => n !== id) : [...nodes, id] }
    })

  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <Button onClick={() => setEditing({ name: "", target: "", interval: 60, nodes: [] })}>
          <Plus /> 添加监控
        </Button>
      </div>

      <Card className="overflow-x-auto p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-[24%]">名称</TableHead>
              <TableHead className="w-[40%]">目标</TableHead>
              <TableHead className="w-[12%]">间隔</TableHead>
              <TableHead className="w-[12%]">节点</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {tasks.map((t) => (
              <TableRow key={t.id}>
                <TableCell className="font-medium">{t.name}</TableCell>
                <TableCell className="tnum text-sm">{t.target}</TableCell>
                <TableCell className="tnum text-sm">{t.interval}s</TableCell>
                <TableCell className="text-sm text-muted-foreground">{t.nodes.length} 个</TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  <Button variant="ghost" size="icon" onClick={() => setEditing(t)} title="编辑监控" aria-label="编辑监控"><Pencil /></Button>
                  <Button variant="ghost" size="icon" onClick={() => setDeleting(t)} title="删除监控" aria-label="删除监控">
                    <Trash2 className="text-destructive" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
            {tasks.length === 0 && (
              <TableRow>
                <TableCell colSpan={5} className="py-10 text-center text-sm text-muted-foreground">
                  还没有延迟监控。每个节点独立 TCP 连接目标端口并上报耗时。
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      {editing && (
        <Dialog open onOpenChange={(open) => !open && setEditing(null)}>
          <DialogContent onOpenAutoFocus={(e) => e.preventDefault()} className="sm:max-w-xl">
            <DialogHeader>
              <DialogTitle>{editing.id ? "编辑监控" : "添加监控"}</DialogTitle>
            </DialogHeader>
            <div className="space-y-5">
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label="名称">
                  {/* A new monitor starts empty, so the cursor belongs here;
                      editing an existing one starts with nothing selected. */}
                  <Input autoFocus={!editing.id} value={editing.name ?? ""} onChange={(e) => setEditing({ ...editing, name: e.target.value })} placeholder="Cloudflare" />
                </Field>
                <Field label="间隔（秒）" hint="5–3600">
                  <Input type="number" min="5" max="3600" value={editing.interval ?? 60} onChange={(e) => setEditing({ ...editing, interval: Number(e.target.value) })} />
                </Field>
              </div>
              <Field label="目标地址" hint="host:port">
                <Input value={editing.target ?? ""} onChange={(e) => setEditing({ ...editing, target: e.target.value })} placeholder="1.1.1.1:443" />
              </Field>
              <div className="space-y-2">
                <Label className="text-sm font-medium">运行节点</Label>
                <div className="max-h-48 space-y-1 overflow-y-auto rounded-lg border bg-muted/20 p-2">
                  {nodes.map((n) => (
                    <label key={n.id} className="flex cursor-pointer items-center gap-2 rounded-md px-2.5 py-2 text-sm hover:bg-background">
                      <input type="checkbox" checked={editing.nodes?.includes(n.id) ?? false} onChange={() => toggle(n.id)} className="accent-primary" />
                      {n.name}
                    </label>
                  ))}
                  {nodes.length === 0 && <p className="p-2 text-xs text-muted-foreground">先添加节点</p>}
                </div>
              </div>
            </div>
            <DialogFooter>
              <Button variant="ghost" onClick={() => setEditing(null)}>取消</Button>
              <Button onClick={save} disabled={saving}>保存</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
      )}
      {deleting && (
        <ConfirmDialog
          title={`删除监控「${deleting.name}」？`}
          description="该监控及其历史延迟记录一并删除，不可恢复。"
          confirmLabel="删除监控"
          busy={removing}
          onClose={() => setDeleting(null)}
          onConfirm={remove}
        />
      )}
    </div>
  )
}

type Theme = {
  name: string
  short: string
  description: string
  version: string
  author: string
  url: string
  selected: boolean
}

function Themes() {
  const [themes, setThemes] = useState<Theme[] | null>(null)
  const [busy, setBusy] = useState("")
  const [doomed, setDoomed] = useState<Theme | null>(null)
  const [zoomed, setZoomed] = useState<Theme | null>(null)
  const picker = useRef<HTMLInputElement>(null)

  const load = () =>
    api<{ themes: Theme[] }>("/themes").then((data) => setThemes(data.themes)).catch(() => setThemes([]))
  useEffect(() => { load() }, [])

  async function select(short: string) {
    try {
      await api("/settings", { method: "PUT", body: JSON.stringify({ theme: short }) })
      setThemes((old) => old?.map((theme) => ({ ...theme, selected: theme.short === short })) ?? old)
      toast.success("主题已切换")
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  async function install(file: File) {
    setBusy("upload")
    try {
      const { theme } = await upload<{ theme: Theme }>("/themes", file)
      // The hub reads a theme off disk on every request, so it is already
      // live -- reloading the list is only so this page catches up.
      toast.success(`已安装 ${theme.name} ${theme.version}`)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
    }
  }

  // Only a theme whose manifest points at a GitHub repository has somewhere
  // to update from; the hub refuses anything else, this just hides the button.
  const updatable = (theme: Theme) =>
    theme.short !== "default" && theme.url.startsWith("https://github.com/")

  async function update(theme: Theme) {
    setBusy(`update:${theme.short}`)
    try {
      const { updated, version } = await api<{ updated: boolean; version: string }>(
        `/themes/${theme.short}/update`,
        { method: "POST" },
      )
      toast.success(updated ? `${theme.name} 已更新到 ${version}` : `${theme.name} 已是最新版本 ${version}`)
      if (updated) load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
    }
  }

  async function remove(theme: Theme) {
    setBusy("delete")
    try {
      await api(`/themes/${theme.short}`, { method: "DELETE" })
      toast.success(`已删除 ${theme.name}`)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
      setDoomed(null)
    }
  }

  if (!themes) return null
  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">安装主题</h3>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            上传主题作者发布的 <code>theme.tar.gz</code>，同名主题整体替换。
            <br />
            主题的 <code>url</code> 指向 GitHub 仓库时，卡片上的 <RefreshCw className="inline size-3" /> 从它最新的
            release 取 <code>theme.tar.gz</code>，版本没变就不下载。
            <br />
            主题代码在访客浏览器中执行，请只安装可信来源。
          </p>
        </div>
        <div>
          <Button size="sm" disabled={!!busy} onClick={() => picker.current?.click()}>
            <Upload /> {busy === "upload" ? "安装中…" : "上传主题包"}
          </Button>
          <input
            ref={picker}
            type="file"
            accept=".gz,.tgz,application/gzip"
            className="hidden"
            onChange={(e) => {
              const file = e.target.files?.[0]
              e.target.value = ""
              if (file) install(file)
            }}
          />
        </div>
      </Card>

      {/* items-start：有预览图和没有的卡片不该为了等高而留白 */}
      <div className="grid items-start gap-3 sm:grid-cols-2">
        {themes.map((theme) => (
          <Card key={theme.short} className="gap-4 p-5">
            {/* 主题包里可选的 preview.png，所以后端不用告诉前端有没有这张图：
                没有就是 404，图一直不显示。hidden 挂在 <a> 上而不是 <img> 上——
                隐藏的是整个链接，否则卡片里留着一个高度为 0 却照样吃 gap-4 的空
                链接。必须从 hidden 开始：带边框的 aspect-video 空盒子会在响应回
                来之前就画出来，闪一下再消失。
                缩略图被压到卡片那点宽度，比例不是 16:9 的还会被 object-cover
                裁掉边，所以图本身要能点开看原尺寸——就地开一个对话框，不跳走。 */}
            <button
              type="button"
              title="查看完整预览图"
              hidden
              className="cursor-zoom-in"
              onClick={() => setZoomed(theme)}
            >
              <img
                src={`/api/themes/${theme.short}/preview`}
                alt={`${theme.name} 预览图`}
                onLoad={(e) => { e.currentTarget.parentElement!.hidden = false }}
                className="aspect-video w-full rounded-md border object-cover object-top"
              />
            </button>
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <h3 className="font-medium">{theme.name}</h3>
                  {theme.selected && <Badge>当前</Badge>}
                </div>
                <p className="mt-1 text-sm text-muted-foreground">{theme.description}</p>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <Button size="sm" variant={theme.selected ? "secondary" : "default"} disabled={theme.selected} onClick={() => select(theme.short)}>
                  {theme.selected ? "使用中" : "使用"}
                </Button>
                {updatable(theme) && (
                  <Button
                    size="icon"
                    variant="ghost"
                    title="从 GitHub 更新"
                    disabled={!!busy}
                    onClick={() => update(theme)}
                  >
                    <RefreshCw className={busy === `update:${theme.short}` ? "animate-spin" : ""} />
                  </Button>
                )}
                {/* The built-in theme is served from the binary and has no
                    directory to delete -- it is also the fallback everything
                    else lands on. */}
                {theme.short !== "default" && (
                  <Button size="icon" variant="ghost" disabled={!!busy} onClick={() => setDoomed(theme)}>
                    <Trash2 />
                  </Button>
                )}
              </div>
            </div>
            <p className="text-xs text-muted-foreground">
              {theme.author} · {theme.version}
              {theme.url && <> · <a className="hover:underline" href={theme.url} target="_blank" rel="noreferrer">源码</a></>}
            </p>
          </Card>
        ))}
      </div>

      {/* 原图，不是卡片上那张裁过的：宽度给到 4xl，高度让 80vh 兜住，
          object-contain 保证整张都在框里而不是被切一刀。 */}
      {zoomed && (
        <Dialog open onOpenChange={(open) => !open && setZoomed(null)}>
          <DialogContent className="sm:max-w-4xl">
            <DialogHeader>
              <DialogTitle>{zoomed.name} 预览图</DialogTitle>
              <DialogDescription>{zoomed.author} · {zoomed.version}</DialogDescription>
            </DialogHeader>
            <img
              src={`/api/themes/${zoomed.short}/preview`}
              alt={`${zoomed.name} 预览图`}
              className="max-h-[80vh] w-full rounded-md border object-contain"
            />
          </DialogContent>
        </Dialog>
      )}

      {doomed && (
        <ConfirmDialog
          title={`删除 ${doomed.name}？`}
          description={
            doomed.selected
              ? "这是当前使用的主题，删除后公开页会回到内置的默认主题。"
              : "主题目录会从磁盘上删掉，重新上传主题包可以装回来。"
          }
          confirmLabel="删除"
          busy={!!busy}
          onClose={() => setDoomed(null)}
          onConfirm={() => remove(doomed)}
        />
      )}
    </div>
  )
}

type Settings = Record<string, string | boolean>

// Two pages write settings -- 设置 and 安全 -- and each loads only what it
// shows.
function useSettings() {
  const [s, setS] = useState<Settings | null>(null)
  useEffect(() => { api<Settings>("/settings").then(setS).catch(() => {}) }, [])
  return {
    s,
    set: (k: string, v: string) => setS((old) => ({ ...(old ?? {}), [k]: v })),
    save: async (patch: Record<string, string>) => {
      try {
        await api("/settings", { method: "PUT", body: JSON.stringify(patch) })
        toast.success("已保存")
      } catch (e) {
        toast.error((e as Error).message)
      }
    },
  }
}

function SettingsTab() {
  const { s, set, save } = useSettings()
  if (!s) return null

  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="站点名称">
            <Input value={String(s.site_name ?? "")} onChange={(e) => set("site_name", e.target.value)} placeholder="Monitor" />
          </Field>
          <Field label="历史数据保留天数" hint="超出的明细自动清理，累计流量不受影响">
            <Input
              type="number"
              value={String(s.retention_days ?? "")}
              onChange={(e) => set("retention_days", e.target.value)}
              placeholder="30"
            />
          </Field>
          <Field label="GitHub 代理" hint="留空直连。仅在 hub 自己拉不到 GitHub Release 时填">
            <Input
              value={String(s.github_proxy ?? "")}
              onChange={(e) => set("github_proxy", e.target.value)}
              placeholder="https://ghfast.top"
            />
          </Field>
        </div>
        {/* 不是 <label>：点文字不该切换开关，只有开关自己可点。
            aria-labelledby 保住读屏软件那边的关联。 */}
        <div className="flex items-center gap-2 text-sm">
          <Switch
            aria-labelledby="public-page-label"
            checked={s.public_page !== "off"}
            onCheckedChange={(v) => set("public_page", v ? "on" : "off")}
          />
          <span id="public-page-label">开放公开状态页，关闭后所有页面需登录</span>
        </div>
        <div>
          <Button
            size="sm"
            onClick={() =>
              save({
                site_name: String(s.site_name ?? ""),
                retention_days: String(s.retention_days ?? "30"),
                github_proxy: String(s.github_proxy ?? ""),
                public_page: s.public_page === "off" ? "off" : "on",
              })
            }
          >
            保存站点设置
          </Button>
        </div>
      </Card>
    </div>
  )
}

// The two ways into this panel, on their own page: the GitHub identity it
// trusts and the password that still works when GitHub does not.
type Session = { id: string; current: boolean; created_at: number }

function Sessions() {
  const [rows, setRows] = useState<Session[] | null>(null)
  const [busy, setBusy] = useState("")

  const load = () => api<Session[]>("/sessions").then(setRows).catch((e: Error) => toast.error(e.message))
  useEffect(() => { load() }, [])

  async function remove(id: string) {
    setBusy(id)
    try {
      await api(`/sessions/${id}`, { method: "DELETE" })
      toast.success("已删除会话")
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
    }
  }

  if (!rows) return null
  return (
    <Card className="gap-4 p-5">
      <div>
        <h3 className="text-sm font-medium">登录会话</h3>
        <p className="mt-1 text-xs text-muted-foreground">
          每次登录一条，14 天后过期。删除后该设备下一次请求就被登出。
        </p>
      </div>
      <div className="divide-y">
        {rows.map((s) => (
          <div key={s.id} className="flex items-center justify-between gap-3 py-2.5 first:pt-0 last:pb-0">
            <div className="flex min-w-0 items-center gap-2 text-sm">
              <span className="tnum">{new Date(s.created_at * 1000).toLocaleString()}</span>
              {s.current && <Badge variant="secondary">当前设备</Badge>}
            </div>
            {/* 当前会话没有删除按钮：右上角的退出登录做的就是这件事，而在这里删
                只会让已经渲染好的面板以为自己还登着。 */}
            {!s.current && (
              <Button size="icon" variant="ghost" disabled={!!busy} onClick={() => remove(s.id)}>
                <Trash2 />
              </Button>
            )}
          </div>
        ))}
      </div>
    </Card>
  )
}

function Security({ site }: { site: string }) {
  const { s, set, save } = useSettings()
  const [password, setPassword] = useState("")
  if (!s) return null
  const callback = `${site}/api/auth/github/callback`

  return (
    <div className="space-y-4">
      <Sessions />

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">GitHub 单点登录</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            OAuth App 回调地址 <code className="rounded bg-muted px-1">{callback}</code>
          </p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Client ID">
            <Input value={String(s.github_client_id ?? "")} onChange={(e) => set("github_client_id", e.target.value)} />
          </Field>
          <Field label="Client Secret" hint={s.github_secret_set ? "已设置，留空不变" : "未设置"}>
            <Input type="password" placeholder={s.github_secret_set ? "••••••••" : ""} onChange={(e) => set("github_client_secret", e.target.value)} />
          </Field>
        </div>
        {String(s.github_client_id ?? "") !== "" && String(s.github_allowed_users ?? "").trim() === "" && (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
            白名单为空，GitHub 登录拒绝所有人。填入用户名并保存后生效。
          </p>
        )}
        <Field label="允许登录的 GitHub 用户名" hint="逗号分隔。留空 = 拒绝所有人，不是放行所有人">
          <Input value={String(s.github_allowed_users ?? "")} onChange={(e) => set("github_allowed_users", e.target.value)} placeholder="GitHub 用户名" />
        </Field>
        <div>
          <Button
            size="sm"
            onClick={() => {
              const patch: Record<string, string> = {
                github_client_id: String(s.github_client_id ?? ""),
                github_allowed_users: String(s.github_allowed_users ?? ""),
              }
              if (typeof s.github_client_secret === "string" && s.github_client_secret) {
                patch.github_client_secret = s.github_client_secret
              }
              save(patch)
            }}
          >
            保存 GitHub 设置
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">应急密码</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            GitHub 不可用时的备用入口。修改后其它设备登录立即失效，当前设备不受影响。
          </p>
        </div>
        <Field label="新密码" hint="至少 12 位">
          <Input type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="new-password" />
        </Field>
        <div>
          <Button
            size="sm"
            disabled={password.length < 12}
            onClick={() => save({ admin_password: password }).then(() => setPassword(""))}
          >
            修改密码
          </Button>
        </div>
      </Card>
    </div>
  )
}

type DbInfo = {
  path: string
  size: number
  wal: number
  free: number
  /** Timestamp of the earliest history row, null on a database with none. */
  oldest: number | null
  retention: number
  rows: Record<string, number>
}

// The only two tables whose row count says anything about size. Every other
// one is a row per node or per key.
const DB_ROWS: [string, string][] = [
  ["metric", "历史明细"],
  ["ping_record", "延迟记录"],
]

function Data() {
  const [info, setInfo] = useState<DbInfo | null>(null)
  const [busy, setBusy] = useState("")
  const [confirm, setConfirm] = useState<"vacuum" | null>(null)
  const [pending, setPending] = useState<File | null>(null)
  const [sent, setSent] = useState(0)
  const picker = useRef<HTMLInputElement>(null)

  const load = () => api<DbInfo>("/db").then(setInfo).catch((e: Error) => toast.error(e.message))
  useEffect(() => { load() }, [])

  async function vacuum() {
    setBusy("vacuum")
    try {
      const { pruned, freed } = await api<{ pruned: number; freed: number }>("/db/vacuum", { method: "POST" })
      toast.success(`已清理 ${pruned} 行，回收 ${bytes(freed)}`)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setBusy("")
      setConfirm(null)
    }
  }

  async function restore(file: File) {
    setBusy("restore")
    setSent(0)
    try {
      await upload("/db/restore", file, setSent)
      toast.success("已恢复，正在重新加载")
      // Every node, setting and session on the page came from the database
      // that was just replaced.
      setTimeout(() => location.reload(), 800)
    } catch (e) {
      toast.error((e as Error).message)
      setBusy("")
    }
    setPending(null)
  }

  if (!info) return null
  const stat = (label: string, value: string) => (
    <div key={label}>
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="tnum mt-0.5 text-sm">{value}</div>
    </div>
  )

  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <h3 className="text-sm font-medium">数据库</h3>
        <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
          {stat("文件大小", bytes(info.size))}
          {stat("预写日志", bytes(info.wal))}
          {stat("可回收空间", bytes(info.free))}
          {stat("保留天数", `${info.retention} 天`)}
          {/* 和保留天数并排：跨度小于保留期是还没攒够，大于保留期就是每小时
              那次 prune 没在跑。 */}
          {stat("历史跨度", info.oldest ? `${Math.floor((Date.now() / 1000 - info.oldest) / 86400)} 天` : "—")}
          {DB_ROWS.map(([key, label]) => stat(label, (info.rows[key] ?? 0).toLocaleString()))}
        </div>
        <p className="truncate text-xs text-muted-foreground" title={info.path}>
          <code>{info.path}</code>
        </p>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">回收空间</h3>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            按保留天数清掉过期明细，再重建数据库文件把空出来的页还给磁盘（SQLite 的 VACUUM）。
            重建期间需要与数据库等量的空闲磁盘，过程中面板和上报会短暂变慢。
          </p>
        </div>
        <div>
          <Button size="sm" variant="secondary" disabled={!!busy} onClick={() => setConfirm("vacuum")}>
            {busy === "vacuum" ? "回收中…" : "立即回收"}
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">备份</h3>
          <p className="mt-1 text-xs leading-relaxed text-muted-foreground">
            导出的是整个数据库，含节点凭证与登录密码哈希，请当作密钥保管。恢复会用备份文件整体覆盖当前数据，
            当前节点、设置、历史全部作废，所有设备需要重新登录。
            <br />
            请用这里导出的文件恢复：直接复制 <code>monitor.db</code> 会丢掉预写日志里还没落盘的那部分。
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {/* The browser's own download: the file is streamed straight from
              the response, never held in the page. */}
          <Button size="sm" asChild>
            <a href="/api/db/backup" download>
              <Download /> 导出备份
            </a>
          </Button>
          <Button size="sm" variant="secondary" disabled={!!busy} onClick={() => picker.current?.click()}>
            <Upload /> 导入备份
          </Button>
          <input
            ref={picker}
            type="file"
            accept=".db,application/octet-stream"
            className="hidden"
            onChange={(e) => {
              setPending(e.target.files?.[0] ?? null)
              e.target.value = ""
            }}
          />
        </div>
      </Card>

      {confirm === "vacuum" && (
        <ConfirmDialog
          title="回收空间？"
          description="超出保留天数的历史明细会被删除，然后重建数据库文件。累计流量不受影响。"
          confirmLabel="开始回收"
          busy={!!busy}
          onClose={() => setConfirm(null)}
          onConfirm={vacuum}
        />
      )}
      {pending && (
        <ConfirmDialog
          title="用备份覆盖当前数据？"
          description={`将用 ${pending.name}（${bytes(pending.size)}）整体替换当前数据库。当前的节点、设置和历史全部丢失，且无法撤销。`}
          confirmLabel={busy === "restore" ? `已上传 ${bytes(sent)} / ${bytes(pending.size)}` : "确认恢复"}
          busy={!!busy}
          onClose={() => setPending(null)}
          onConfirm={() => restore(pending)}
        />
      )}
    </div>
  )
}

// Each area is its own route rather than a tab, so a page can be linked to
// and a reload lands on the section it was on.
const ADMIN_SECTIONS = [
  { path: "/admin/nodes", label: "节点", icon: Server },
  { path: "/admin/ping", label: "延迟", icon: Radio },
  { path: "/admin/data", label: "数据", icon: Database },
  { path: "/admin/themes", label: "主题", icon: Palette },
  { path: "/admin/security", label: "安全", icon: Shield },
  { path: "/admin/settings", label: "设置", icon: Settings },
] as const

export function Admin({
  path,
  go,
  nodes,
  refresh,
  site,
  canProvision,
}: {
  path: string
  go: (to: string) => void
  nodes: Node[]
  refresh: () => void
  site: string
  canProvision: boolean
}) {
  return (
    <div className="flex flex-col gap-6 md:flex-row">
      <nav className="flex gap-1 overflow-x-auto md:w-44 md:shrink-0 md:flex-col md:overflow-visible">
        {ADMIN_SECTIONS.map(({ path: to, label, icon: Icon }) => {
          const active = path === to
          return (
            <button
              key={to}
              onClick={() => go(to)}
              aria-current={active ? "page" : undefined}
              className={`flex shrink-0 items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors ${
                active ? "bg-secondary font-medium" : "text-muted-foreground hover:bg-muted"
              }`}
            >
              <Icon className="size-4" />
              {label}
            </button>
          )
        })}
      </nav>

      <div className="min-w-0 flex-1">
        {path === "/admin/ping" ? (
          <Ping nodes={nodes} />
        ) : path === "/admin/data" ? (
          <Data />
        ) : path === "/admin/themes" ? (
          <Themes />
        ) : path === "/admin/security" ? (
          <Security site={site} />
        ) : path === "/admin/settings" ? (
          <SettingsTab />
        ) : (
          <Nodes nodes={nodes} refresh={refresh} site={site} canProvision={canProvision} />
        )}
      </div>
    </div>
  )
}
