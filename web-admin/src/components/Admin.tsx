import { useEffect, useState } from "react"
import { Copy, KeyRound, Palette, Pencil, Plus, Radio, Server, Settings, Trash2 } from "lucide-react"
import { toast } from "sonner"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card } from "@/components/ui/card"
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, type Node, type PingTask } from "@/lib/api"
import { bytes, CYCLES } from "@/lib/format"

const GIB = 1024 ** 3
const TRAFFIC_MODES: Record<string, string> = {
  sum: "上下行相加",
  max: "取较大值",
  up: "仅上行",
  down: "仅下行",
}

function copy(text: string) {
  navigator.clipboard.writeText(text).then(
    () => toast.success("已复制到剪贴板"),
    () => toast.error("复制失败，请手动选择"),
  )
}

function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1.5">
      <Label className="text-xs">{label}</Label>
      {children}
      {hint && <p className="text-[11px] text-muted-foreground">{hint}</p>}
    </div>
  )
}

const BLANK: Partial<Node> = {
  name: "",
  public: true,
  sort: 0,
  price: 0,
  currency: "USD",
  billing_cycle: "monthly",
  expires_at: "",
  remark: "",
  traffic_limit: 0,
  traffic_mode: "sum",
  traffic_reset_day: 1,
}

function NodeForm({ node, onClose, onSaved }: {
  node: Partial<Node> | null
  onClose: () => void
  onSaved: (created?: { token: string; install: string }) => void
}) {
  const [form, setForm] = useState<Partial<Node>>(node ?? BLANK)
  const [limitGib, setLimitGib] = useState(String((node?.traffic_limit ?? 0) / GIB || ""))
  const [saving, setSaving] = useState(false)
  const set = <K extends keyof Node>(k: K, v: Node[K]) => setForm((f) => ({ ...f, [k]: v }))

  async function save() {
    if (!form.name?.trim()) return toast.error("请填写节点名称")
    setSaving(true)
    const body = {
      ...form,
      traffic_limit: Math.round((Number(limitGib) || 0) * GIB),
      expires_at: form.expires_at || null,
      price: Number(form.price) || 0,
      sort: Number(form.sort) || 0,
      traffic_reset_day: Math.min(31, Math.max(1, Number(form.traffic_reset_day) || 1)),
    }
    try {
      if (node?.id) {
        await api(`/nodes/${node.id}`, { method: "PUT", body: JSON.stringify(body) })
        toast.success("已保存")
        onSaved()
      } else {
        const created = await api<{ token: string; install: string }>("/nodes", {
          method: "POST",
          body: JSON.stringify(body),
        })
        onSaved(created)
      }
      onClose()
    } catch (e) {
      toast.error((e as Error).message)
    } finally {
      setSaving(false)
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-h-[92vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{node?.id ? "编辑节点" : "添加节点"}</DialogTitle>
        </DialogHeader>

        <div className="grid gap-4 sm:grid-cols-2">
          <Field label="名称">
            <Input value={form.name ?? ""} onChange={(e) => set("name", e.target.value)} placeholder="香港 · 甲商家" />
          </Field>
          <Field label="排序" hint="数字小的排在前面">
            <Input type="number" value={form.sort ?? 0} onChange={(e) => set("sort", Number(e.target.value))} />
          </Field>

          <Field label="价格">
            <Input type="number" step="0.01" value={form.price ?? 0} onChange={(e) => set("price", Number(e.target.value))} />
          </Field>
          <Field label="货币">
            <Select value={form.currency} onValueChange={(v) => set("currency", v)}>
              <SelectTrigger><SelectValue /></SelectTrigger>
              <SelectContent>
                {["USD", "CNY", "EUR", "GBP", "JPY"].map((c) => (
                  <SelectItem key={c} value={c}>{c}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          <Field label="付款周期">
            <Select value={form.billing_cycle} onValueChange={(v) => set("billing_cycle", v)}>
              <SelectTrigger><SelectValue /></SelectTrigger>
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

          <Field label="每月流量额度 (GiB)" hint="留空或 0 表示不限">
            <Input type="number" value={limitGib} onChange={(e) => setLimitGib(e.target.value)} placeholder="1024" />
          </Field>
          <Field label="流量计算方式">
            <Select value={form.traffic_mode} onValueChange={(v) => set("traffic_mode", v)}>
              <SelectTrigger><SelectValue /></SelectTrigger>
              <SelectContent>
                {Object.entries(TRAFFIC_MODES).map(([k, v]) => (
                  <SelectItem key={k} value={k}>{v}</SelectItem>
                ))}
              </SelectContent>
            </Select>
          </Field>

          <Field label="每月重置日" hint="商家的流量重置日，1–31">
            <Input
              type="number" min={1} max={31}
              value={form.traffic_reset_day ?? 1}
              onChange={(e) => set("traffic_reset_day", Number(e.target.value))}
            />
          </Field>
          <div className="flex items-end pb-1.5">
            <label className="flex cursor-pointer items-center gap-2 text-sm">
              <Switch checked={form.public ?? true} onCheckedChange={(v) => set("public", v)} />
              在公开页显示
            </label>
          </div>

          <div className="sm:col-span-2">
            <Field label="备注" hint="仅管理员可见，不会出现在公开页">
              <Input value={form.remark ?? ""} onChange={(e) => set("remark", e.target.value)} />
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

function InstallDialog({ install, onClose }: { install: string; onClose: () => void }) {
  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>在目标 VPS 上执行</DialogTitle>
        </DialogHeader>
        <pre className="overflow-x-auto rounded-md bg-muted p-3 text-xs leading-relaxed select-all">{install}</pre>
        <p className="text-xs text-muted-foreground">
          Token 只显示这一次。若丢失，可在节点列表重新生成 —— 重新生成会立刻让旧 token 失效。
        </p>
        <DialogFooter>
          <Button onClick={() => copy(install)}>
            <Copy /> 复制命令
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

function Nodes({ nodes, refresh }: { nodes: Node[]; refresh: () => void }) {
  const [editing, setEditing] = useState<Partial<Node> | null>(null)
  const [install, setInstall] = useState<string | null>(null)

  async function remove(node: Node) {
    if (!confirm(`删除节点「${node.name}」？历史数据和流量记录会一并删除，且无法恢复。`)) return
    try {
      await api(`/nodes/${node.id}`, { method: "DELETE" })
      toast.success("已删除")
      refresh()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  async function reissue(node: Node) {
    if (!confirm(`为「${node.name}」重新生成 token？旧 token 会立即失效，该节点需要重装 agent。`)) return
    try {
      const { install } = await api<{ install: string }>(`/nodes/${node.id}/token`, { method: "POST" })
      setInstall(install)
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex justify-end">
        <Button onClick={() => setEditing(BLANK)}>
          <Plus /> 添加节点
        </Button>
      </div>

      <Card className="overflow-x-auto p-0">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>名称</TableHead>
              <TableHead>状态</TableHead>
              <TableHead>本月 / 额度</TableHead>
              <TableHead>价格</TableHead>
              <TableHead>到期</TableHead>
              <TableHead className="text-right">操作</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {nodes.map((n) => (
              <TableRow key={n.id}>
                <TableCell>
                  <div className="font-medium">{n.name}</div>
                  <div className="text-xs text-muted-foreground">{n.ip || "—"}</div>
                </TableCell>
                <TableCell>
                  <Badge variant={n.online ? "default" : "secondary"} className="font-normal">
                    {n.online ? "在线" : "离线"}
                  </Badge>
                  {!n.public && <Badge variant="outline" className="ml-1 font-normal">不公开</Badge>}
                </TableCell>
                <TableCell className="tnum text-sm">
                  {bytes(n.month_rx + n.month_tx)}
                  <span className="text-muted-foreground">
                    {" / "}{n.traffic_limit > 0 ? bytes(n.traffic_limit) : "不限"}
                  </span>
                </TableCell>
                <TableCell className="tnum text-sm">
                  {n.price > 0 ? `${n.price} ${n.currency}` : "—"}
                </TableCell>
                <TableCell className="text-sm">{n.expires_at || "—"}</TableCell>
                <TableCell className="text-right whitespace-nowrap">
                  <Button variant="ghost" size="icon" onClick={() => reissue(n)} title="重新生成 token">
                    <KeyRound />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => setEditing(n)} title="编辑">
                    <Pencil />
                  </Button>
                  <Button variant="ghost" size="icon" onClick={() => remove(n)} title="删除">
                    <Trash2 className="text-destructive" />
                  </Button>
                </TableCell>
              </TableRow>
            ))}
            {nodes.length === 0 && (
              <TableRow>
                <TableCell colSpan={6} className="py-10 text-center text-sm text-muted-foreground">
                  还没有节点，点右上角添加一个
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </Card>

      {editing && (
        <NodeForm
          node={editing}
          onClose={() => setEditing(null)}
          onSaved={(created) => {
            refresh()
            if (created) setInstall(created.install)
          }}
        />
      )}
      {install && <InstallDialog install={install} onClose={() => setInstall(null)} />}
    </div>
  )
}

function Ping({ nodes }: { nodes: Node[] }) {
  const [tasks, setTasks] = useState<PingTask[]>([])
  const [editing, setEditing] = useState<Partial<PingTask> | null>(null)

  const load = () => api<{ tasks: PingTask[] }>("/ping-tasks").then((d) => setTasks(d.tasks)).catch(() => {})
  useEffect(() => { load() }, [])

  async function save() {
    if (!editing) return
    try {
      await api("/ping-tasks", { method: "POST", body: JSON.stringify(editing) })
      toast.success("已保存，正在下发到 agent")
      setEditing(null)
      load()
    } catch (e) {
      toast.error((e as Error).message)
    }
  }

  async function remove(task: PingTask) {
    if (!confirm(`删除监控「${task.name}」及其历史记录？`)) return
    await api(`/ping-tasks/${task.id}`, { method: "DELETE" })
    load()
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
                  <Button variant="ghost" size="icon" onClick={() => setEditing(t)}><Pencil /></Button>
                  <Button variant="ghost" size="icon" onClick={() => remove(t)}>
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
          <DialogContent className="max-h-[92vh] overflow-y-auto sm:max-w-lg">
            <DialogHeader>
              <DialogTitle>{editing.id ? "编辑监控" : "添加监控"}</DialogTitle>
            </DialogHeader>
            <Field label="名称">
              <Input value={editing.name ?? ""} onChange={(e) => setEditing({ ...editing, name: e.target.value })} placeholder="Cloudflare" />
            </Field>
            <Field label="目标" hint="必须是 host:port，例如 1.1.1.1:443 或 www.google.com:80">
              <Input value={editing.target ?? ""} onChange={(e) => setEditing({ ...editing, target: e.target.value })} placeholder="1.1.1.1:443" />
            </Field>
            <Field label="间隔（秒）" hint="5 到 3600">
              <Input type="number" value={editing.interval ?? 60} onChange={(e) => setEditing({ ...editing, interval: Number(e.target.value) })} />
            </Field>
            <Field label="应用到节点">
              <div className="max-h-48 space-y-1 overflow-y-auto rounded-md border p-2">
                {nodes.map((n) => (
                  <label key={n.id} className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 text-sm hover:bg-muted">
                    <input
                      type="checkbox"
                      checked={editing.nodes?.includes(n.id) ?? false}
                      onChange={() => toggle(n.id)}
                      className="accent-primary"
                    />
                    {n.name}
                  </label>
                ))}
                {nodes.length === 0 && <p className="p-2 text-xs text-muted-foreground">先添加节点</p>}
              </div>
            </Field>
            <DialogFooter>
              <Button variant="ghost" onClick={() => setEditing(null)}>取消</Button>
              <Button onClick={save}>保存</Button>
            </DialogFooter>
          </DialogContent>
        </Dialog>
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
