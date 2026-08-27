import { useEffect, useState } from "react"
import { Moon, Sun, Wrench } from "lucide-react"

import { NodeCard } from "@/components/NodeCard"
import { NodeDetail } from "@/components/NodeDetail"
import { Summary } from "@/components/Summary"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { api, useNodes, type Node, type PingTask } from "@/lib/api"

type Me = { authed: boolean; github: boolean; site_name: string; public_page: boolean }

function useTheme() {
  const [dark, setDark] = useState(() => {
    const saved = localStorage.getItem("theme")
    return saved ? saved === "dark" : matchMedia("(prefers-color-scheme: dark)").matches
  })
  useEffect(() => {
    document.documentElement.classList.toggle("dark", dark)
    localStorage.setItem("theme", dark ? "dark" : "light")
  }, [dark])
  return [dark, () => setDark((d) => !d)] as const
}

export default function App() {
  const [dark, toggleTheme] = useTheme()
  const [me, setMe] = useState<Me | null>(null)
  const { nodes, admin, error } = useNodes()
  const [open, setOpen] = useState<number | null>(null)
  const [tasks, setTasks] = useState<PingTask[]>([])

  useEffect(() => {
    api<Me>("/me").then(setMe).catch(() => {})
  }, [])

  // Probe names label the latency chart. Only an admin can read the task list;
  // for everyone else the chart simply falls back to unnamed series.
  useEffect(() => {
    if (admin) api<{ tasks: PingTask[] }>("/ping-tasks").then((d) => setTasks(d.tasks)).catch(() => {})
  }, [admin])

  useEffect(() => {
    if (me && !me.public_page && !me.authed) location.href = "/admin/"
  }, [me])

  if (!me) return <div className="grid min-h-svh place-items-center text-sm text-muted-foreground">加载中…</div>

  // The status page is closed and nobody is signed in: send them to the panel.
  if (!me.public_page && !me.authed) return null

  const sorted = [...(nodes ?? [])].sort((a, b) => a.sort - b.sort || a.id - b.id)
  const selected = sorted.find((n) => n.id === open)

  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-10 border-b bg-background/80 backdrop-blur">
        <div className="mx-auto flex max-w-6xl items-center gap-3 px-4 py-3">
          <span className="font-semibold">{me.site_name || "Monitor"}</span>
          <div className="flex-1" />
          {/* The admin panel is a separate app built into the hub, not part of
              this theme, so this is a real navigation rather than a route. */}
          <Button variant="ghost" size="sm" asChild>
            <a href="/admin/">
              <Wrench /> {me.authed ? "进入后台" : "登录"}
            </a>
          </Button>
          <Button variant="ghost" size="icon" onClick={toggleTheme} title="切换主题">
            {dark ? <Sun /> : <Moon />}
          </Button>
        </div>
      </header>

      <main className="mx-auto max-w-6xl space-y-5 px-4 py-6">
        {error && <p className="text-sm text-destructive">{error}</p>}

        {!nodes ? (
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {[0, 1, 2].map((i) => (
              <Skeleton key={i} className="h-72" />
            ))}
          </div>
        ) : (
          <>
            <Summary nodes={sorted} />
            {sorted.length === 0 ? (
              <p className="py-16 text-center text-sm text-muted-foreground">还没有节点</p>
            ) : (
              <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                {sorted.map((n: Node) => (
                  <NodeCard key={n.id} node={n} onOpen={() => setOpen(n.id)} />
                ))}
              </div>
            )}
          </>
        )}
      </main>

      {selected && <NodeDetail node={selected} tasks={tasks} onClose={() => setOpen(null)} />}
    </div>
  )
}
