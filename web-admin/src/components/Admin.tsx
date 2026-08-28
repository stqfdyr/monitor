import { useEffect, useRef, useState } from "react"
import { flushSync } from "react-dom"
import { CalendarClock, Copy, Download, GripVertical, Palette, Pencil, Plus, Radio, Server, Settings, Trash2 } from "lucide-react"
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
import { api, type Node, type PingTask } from "@/lib/api"
import { bytes, CYCLES, monthUsage } from "@/lib/format"

const GIB = 1024 ** 3
/// Counters the panel can correct by hand, e.g. after a node moved to new
/// hardware and the hub booked the new machine's lifetime traffic in one go.
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

/// Reordering rides on the browser's own view transitions, so the rows that
/// make way slide instead of jumping. Browsers without it just jump.
function animate(update: () => void) {
  if (document.startViewTransition) document.startViewTransition(() => flushSync(update))
  else update()
}

function copy(text: string) {
  navigator.clipboard.writeText(text).then(
    () => toast.success("已复制到剪贴板"),
    () => toast.error("复制失败，请手动选择"),
  )
}

/// Every address a node has, on one line, each click-to-copy: reading one off
/// the screen to paste into an ssh command is the whole reason it is shown.
function Addresses({ node }: { node: Node }) {
  const reported = [node.ipv4, node.ipv6].filter(Boolean) as string[]
  // `ip` is only where the agent's connection came from — the fallback for an
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
  // Compared as typed, not as bytes: rounding to GiB would otherwise look like
  // an edit and zero out a node that has only moved a few MiB.
  const pristine = useRef(traffic)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))

  async function save() {
    if (!form.name.trim()) return toast.error("请填写节点名称")
    setSaving(true)
    try {
      if (TRAFFIC_FIELDS.some(([k]) => traffic[k] !== pristine.current[k])) {
        await api(`/nodes/${node.id}/traffic`, {
          method: "PUT",
          body: JSON.stringify(
            Object.fromEntries(TRAFFIC_FIELDS.map(([k]) => [k, Math.round((Number(traffic[k]) || 0) * GIB)])),
          ),
        })
      }
      await api(`/nodes/${node.id}`, {
        method: "PUT",
        body: JSON.stringify({
          ...form,
          name: form.name.trim(),
          traffic_limit: Math.round((Number(limitGib) || 0) * GIB),
          traffic_reset_day: Math.min(31, Math.max(1, Number(form.traffic_reset_day) || 1)),
        }),
      })
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
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <Field label="名称">
            <Input value={form.name} onChange={(e) => set("name", e.target.value)} />
          </Field>
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="每月流量额度 (GiB)" hint="留空或 0 表示不限">
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
            <Field label="每月重置日" hint="1–31。改了之后本月流量按新周期从头计，总流量不受影响">
              <Input type="number" min={1} max={31} value={form.traffic_reset_day} onChange={(e) => set("traffic_reset_day", Number(e.target.value))} />
            </Field>
            <Field label="备注" hint="仅管理员可见">
              <Input value={form.remark ?? ""} onChange={(e) => set("remark", e.target.value)} placeholder="商家、用途或其它说明" />
            </Field>
          </div>
          <details className="rounded-lg border bg-muted/30 px-3 py-2.5">
            <summary className="cursor-pointer text-sm font-medium">流量校正</summary>
            <p className="mt-2 text-xs leading-relaxed text-muted-foreground">
              换了机器或重装系统后，hub 会把新机器的历史计数当成一次增量记进来。这里按 GiB 改成正确的数字。
            </p>
            <div className="mt-3 grid gap-4 sm:grid-cols-2">
              {TRAFFIC_FIELDS.map(([key, label]) => (
                <Field key={key} label={`${label} (GiB)`}>
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
  const [saving, setSaving] = useState(false)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))

  async function save() {
    setSaving(true)
    try {
      await api(`/nodes/${node.id}`, {
        method: "PUT",
        body: JSON.stringify({
          ...form,
          price: Number(form.price) || 0,
          expires_at: form.expires_at || null,
        }),
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
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
        </DialogHeader>
        <div className="space-y-5">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="价格">
              <Input type="number" min="0" step="0.01" value={form.price} onChange={(e) => set("price", Number(e.target.value))} />
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

function shellArg(value: string) {
  return `'${value.replaceAll("'", `'"'"'`)}'`
}

type Install = { install: string }

function InstallDialog({ node, onClose }: { node: Node; onClose: () => void }) {
  const [details, setDetails] = useState<Install | null>(null)
  const [interval, setInterval] = useState("1")
  const [githubProxy, setGithubProxy] = useState("")

  useEffect(() => {
    let cancelled = false
    api<Install>(`/nodes/${node.id}/token`, { method: "POST" })
      .then((d) => {
        if (!cancelled) setDetails(d)
      })
      .catch((e) => toast.error((e as Error).message))
    return () => {
      cancelled = true
    }
  }, [node.id])

  const seconds = Math.min(3600, Math.max(1, Math.round(Number(interval) || 1)))
  const args = ["--interval", String(seconds)]
  const proxy = githubProxy.trim()
  if (proxy) args.push("--github-proxy", shellArg(proxy.includes("://") ? proxy : `https://${proxy}`))
  const command = details ? `${details.install} ${args.join(" ")}` : ""

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{node.name}</DialogTitle>
          {/* Opening this dialog already rotated the token and cut the node's
              running agent loose. Saying so beats letting someone wonder why
              a node they only looked at just went offline. */}
          <DialogDescription className="leading-relaxed">
            已换发新凭证，这个节点上原来运行的 agent 随即掉线。用下面的命令重装即可恢复。
          </DialogDescription>
        </DialogHeader>
        <div className="space-y-5">
          <div className="grid gap-4 sm:grid-cols-2">
            <Field label="上报间隔（秒）" hint="1–3600，默认 1 秒">
              <Input type="number" min={1} max={3600} value={interval} onChange={(e) => setInterval(e.target.value)} />
            </Field>
            <Field label="GitHub 代理" hint="仅代理 GitHub Release">
              <Input value={githubProxy} onChange={(e) => setGithubProxy(e.target.value)} placeholder="https://ghfast.top" />
            </Field>
          </div>
          <div className="space-y-2">
            <Label className="text-sm font-medium">安装命令</Label>
            <pre className="h-28 overflow-auto whitespace-pre-wrap break-all rounded-lg border bg-muted/40 p-3 text-xs leading-relaxed select-all">
              {details ? command : "正在生成…"}
            </pre>
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>取消</Button>
          <Button onClick={() => copy(command)} disabled={!details}>
            <Copy className="size-4" /> 复制
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function Nodes({ nodes, refresh }: { nodes: Node[]; refresh: () => void }) {
  const [creating, setCreating] = useState(false)
  const [editing, setEditing] = useState<Node | null>(null)
  const [billing, setBilling] = useState<Node | null>(null)
  const [installing, setInstalling] = useState<Node | null>(null)
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

  /// Rows make way while the pointer is still down; the order is only saved
  /// once it is dropped.
  function move(from: number, to: number) {
    if (from < 0 || to < 0 || to >= order.length || from === to) return
    const next = [...order]
    next.splice(to, 0, ...next.splice(from, 1))
    const ids = next.map((node) => node.id)
    animate(() => setManualOrder(ids))
    return ids
  }

  /// Dropped outside the table, or cancelled with Escape: back where it was.
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
      <div className="flex justify-end">
        <Button onClick={() => setCreating(true)}>
          <Plus /> 添加节点
        </Button>
      </div>

      <Card className="overflow-x-auto p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>名称</TableHead>
              <TableHead>IP</TableHead>
              <TableHead>状态</TableHead>
              <TableHead>本月 / 额度</TableHead>
              <TableHead>价格</TableHead>
              <TableHead>到期</TableHead>
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
                </TableCell>
                {/* Counted by the node's own billing rule, the same way the
                    public page and the quota below it are. */}
                <TableCell className="tnum text-sm">
                  {bytes(monthUsage(n))}
                  <span className="text-muted-foreground">
                    {" / "}{n.traffic_limit > 0 ? bytes(n.traffic_limit) : "不限"}
                  </span>
                </TableCell>
                <TableCell className="tnum text-sm">
                  {n.price > 0 ? `${n.price} ${n.currency}` : "—"}
                </TableCell>
                <TableCell className="text-sm">{n.expires_at || "—"}</TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  <Button variant="ghost" size="icon" onClick={() => setInstalling(n)} title="安装 Agent" aria-label="安装 Agent">
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
                  还没有节点，点右上角添加一个
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
      {installing && <InstallDialog node={installing} onClose={() => setInstalling(null)} />}
      {deleting && (
        <ConfirmDialog
          title={`删除节点「${deleting.name}」？`}
          description="历史指标、流量记录和节点凭证都会一并删除，且无法恢复。"
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
      toast.success("已保存，正在下发到 agent")
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
              <TableHead>名称</TableHead>
              <TableHead>目标</TableHead>
              <TableHead>间隔</TableHead>
              <TableHead>节点</TableHead>
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
                  还没有延迟监控。添加后每个节点会独立 TCP 连接目标端口并上报耗时。
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      {editing && (
        <Dialog open onOpenChange={(open) => !open && setEditing(null)}>
          <DialogContent className="sm:max-w-xl">
            <DialogHeader>
              <DialogTitle>{editing.id ? "编辑监控" : "添加监控"}</DialogTitle>
            </DialogHeader>
            <div className="space-y-5">
              <div className="grid gap-4 sm:grid-cols-2">
                <Field label="名称">
                  <Input value={editing.name ?? ""} onChange={(e) => setEditing({ ...editing, name: e.target.value })} placeholder="Cloudflare" />
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
                <p className="text-xs text-muted-foreground">选择由哪些 Agent 执行</p>
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
          description="该监控及其历史延迟记录都会一并删除，且无法恢复。"
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

  useEffect(() => {
    api<{ themes: Theme[] }>("/themes").then((data) => setThemes(data.themes)).catch(() => setThemes([]))
  }, [])

  async function select(short: string) {
    try {
      await api("/settings", { method: "PUT", body: JSON.stringify({ theme: short }) })
      setThemes((old) => old?.map((theme) => ({ ...theme, selected: theme.short === short })) ?? old)
      toast.success("主题已切换")
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  if (!themes) return null
  return (
    <div className="grid gap-3 sm:grid-cols-2">
      {themes.map((theme) => (
        <Card key={theme.short} className="gap-4 p-5">
          <div className="flex items-start gap-3">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <h3 className="font-medium">{theme.name}</h3>
                {theme.selected && <Badge>当前</Badge>}
              </div>
              <p className="mt-1 text-sm text-muted-foreground">{theme.description}</p>
            </div>
            <Button size="sm" variant={theme.selected ? "secondary" : "default"} disabled={theme.selected} onClick={() => select(theme.short)}>
              {theme.selected ? "使用中" : "使用"}
            </Button>
          </div>
          <p className="text-xs text-muted-foreground">
            {theme.author} · {theme.version}
            {theme.url && <> · <a className="hover:underline" href={theme.url} target="_blank" rel="noreferrer">源码</a></>}
          </p>
        </Card>
      ))}
      {themes.length === 0 && <p className="text-sm text-muted-foreground">没有可用主题</p>}
    </div>
  )
}

type Settings = Record<string, string | boolean>

function SettingsTab({ site }: { site: string }) {
  const [s, setS] = useState<Settings | null>(null)
  const [password, setPassword] = useState("")
  const set = (k: string, v: string) => setS((old) => ({ ...(old ?? {}), [k]: v }))

  useEffect(() => { api<Settings>("/settings").then(setS).catch(() => {}) }, [])

  async function save(patch: Record<string, string>) {
    try {
      await api("/settings", { method: "PUT", body: JSON.stringify(patch) })
      toast.success("已保存")
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  if (!s) return null
  const callback = `${site}/api/auth/github/callback`

  return (
    <div className="space-y-4">
      <Card className="gap-4 p-5">
        <h3 className="text-sm font-medium">站点</h3>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="站点名称">
            <Input value={String(s.site_name ?? "")} onChange={(e) => set("site_name", e.target.value)} placeholder="Monitor" />
          </Field>
          <Field label="历史数据保留天数" hint="超出的明细会被自动清理；累计流量不受影响">
            <Input
              type="number"
              value={String(s.retention_days ?? "")}
              onChange={(e) => set("retention_days", e.target.value)}
              placeholder="30"
            />
          </Field>
        </div>
        <label className="flex cursor-pointer items-center gap-2 text-sm">
          <Switch
            checked={s.public_page !== "off"}
            onCheckedChange={(v) => set("public_page", v ? "on" : "off")}
          />
          开放公开状态页（关闭后所有页面都需要登录）
        </label>
        <div>
          <Button
            size="sm"
            onClick={() =>
              save({
                site_name: String(s.site_name ?? ""),
                retention_days: String(s.retention_days ?? "30"),
                public_page: s.public_page === "off" ? "off" : "on",
              })
            }
          >
            保存站点设置
          </Button>
        </div>
      </Card>

      <Card className="gap-4 p-5">
        <div>
          <h3 className="text-sm font-medium">GitHub 单点登录</h3>
          <p className="mt-1 text-xs text-muted-foreground">
            在 GitHub 建一个 OAuth App，回调地址填 <code className="rounded bg-muted px-1">{callback}</code>
          </p>
        </div>
        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="Client ID">
            <Input value={String(s.github_client_id ?? "")} onChange={(e) => set("github_client_id", e.target.value)} />
          </Field>
          <Field label="Client Secret" hint={s.github_secret_set ? "已设置；留空则保持不变" : "尚未设置"}>
            <Input type="password" placeholder={s.github_secret_set ? "••••••••" : ""} onChange={(e) => set("github_client_secret", e.target.value)} />
          </Field>
        </div>
        {String(s.github_client_id ?? "") !== "" && String(s.github_allowed_users ?? "").trim() === "" && (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">
            白名单为空，GitHub 登录现在会拒绝所有人。填上你的 GitHub 用户名并保存后才能用。
          </p>
        )}
        <Field label="允许登录的 GitHub 用户名" hint="逗号分隔。留空 = 拒绝所有人（不是放行所有人）">
          <Input value={String(s.github_allowed_users ?? "")} onChange={(e) => set("github_allowed_users", e.target.value)} placeholder="你的 GitHub 用户名" />
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
            GitHub 不可用时的备用入口。修改后其它设备上的登录会立即失效，当前这台不受影响。
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

/// Each admin area is its own route rather than a tab, so a page can be
/// linked to and survives a reload on the section you were actually in.
const ADMIN_SECTIONS = [
  { path: "/admin/nodes", label: "节点", icon: Server },
  { path: "/admin/ping", label: "延迟监控", icon: Radio },
  { path: "/admin/themes", label: "主题", icon: Palette },
  { path: "/admin/settings", label: "设置", icon: Settings },
] as const

export function Admin({
  path,
  go,
  nodes,
  refresh,
  site,
}: {
  path: string
  go: (to: string) => void
  nodes: Node[]
  refresh: () => void
  site: string
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
        ) : path === "/admin/themes" ? (
          <Themes />
        ) : path === "/admin/settings" ? (
          <SettingsTab site={site} />
        ) : (
          <Nodes nodes={nodes} refresh={refresh} />
        )}
      </div>
    </div>
  )
}
