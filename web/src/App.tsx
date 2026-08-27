import { useCallback, useEffect, useState } from "react"
import { LayoutDashboard, LogOut, Moon, Sun, Wrench } from "lucide-react"
import { Toaster } from "sonner"

import { Admin } from "@/components/Admin"
import { Login } from "@/components/Login"
import { NodeCard } from "@/components/NodeCard"
import { NodeDetail } from "@/components/NodeDetail"
import { Summary } from "@/components/Summary"
import { Button } from "@/components/ui/button"
import { Skeleton } from "@/components/ui/skeleton"
import { api, useNodes, type Node, type PingTask } from "@/lib/api"

type Me = { authed: boolean; github: boolean; site_name: string; public_page: boolean }

/** Two routes and a login screen do not need a router. */
/// `/admin` on its own is not a page; normalise it to the first section so a
/// bookmark, an OAuth redirect and the nav all land somewhere real.
function normalise(p: string) {
  return p === "/admin" || p === "/admin/" ? "/admin/nodes" : p.replace(/\/$/, "") || "/"
}

function usePath() {
  const [path, setPath] = useState(() => {
    const start = normalise(location.pathname)
    if (start !== location.pathname) history.replaceState({}, "", start + location.search)
    return start
  })
  useEffect(() => {
    const sync = () => setPath(normalise(location.pathname))
    addEventListener("popstate", sync)
    return () => removeEventListener("popstate", sync)
  }, [])
  return [
    path,
    useCallback((next: string) => {
      const to = normalise(next)
      history.pushState({}, "", to)
      setPath(to)
    }, []),
  ] as const
}

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
  const [path, go] = usePath()
  const [dark, toggleTheme] = useTheme()
  const [me, setMe] = useState<Me | null>(null)
  const { nodes, admin, error, refresh } = useNodes()
  const [open, setOpen] = useState<number | null>(null)
  const [tasks, setTasks] = useState<PingTask[]>([])

  const loadMe = useCallback(() => api<Me>("/me").then(setMe).catch(() => {}), [])
  useEffect(() => { loadMe() }, [loadMe])

  // Probe names label the latency chart, so the panel's task list is fetched
  // once a session rather than per node.
  useEffect(() => {
    if (admin) api<{ tasks: PingTask[] }>("/ping-tasks").then((d) => setTasks(d.tasks)).catch(() => {})
  }, [admin])

  if (!me) return <div className="grid min-h-svh place-items-center text-sm text-muted-foreground">加载中…</div>

  const isAdmin = path.startsWith("/admin")
  const needsLogin = isAdmin ? !me.authed : !me.public_page && !me.authed
  if (needsLogin) {
    return (
      <>
        <Login github={me.github} onDone={() => { loadMe(); refresh(); go("/admin/nodes") }} />
        <Toaster position="top-center" theme={dark ? "dark" : "light"} />
      </>
    )
  }

  const sorted = [...(nodes ?? [])].sort((a, b) => a.sort - b.sort || a.id - b.id)
  const selected = sorted.find((n) => n.id === open)

  async function signOut() {
    await api("/auth/logout", { method: "POST" }).catch(() => {})
    await loadMe()
    go("/")
    location.reload()
  }

  return (
    <div className="min-h-svh">
      <header className="sticky top-0 z-10 border-b bg-background/80 backdrop-blur">
        <div className="mx-auto flex max-w-6xl items-center gap-3 px-4 py-3">
          <button onClick={() => go("/")} className="font-semibold">
            {me.site_name || "Monitor"}
          </button>
          <div className="flex-1" />
          {me.authed && (
            <Button
              variant={isAdmin ? "secondary" : "ghost"}
              size="sm"
              onClick={() => go(isAdmin ? "/" : "/admin/nodes")}
            >
              {isAdmin ? <LayoutDashboard /> : <Wrench />}
              {isAdmin ? "状态面板" : "进入后台"}
            </Button>
          )}
          <Button variant="ghost" size="icon" onClick={toggleTheme} title="切换主题">
            {dark ? <Sun /> : <Moon />}
          </Button>
          {me.authed ? (
            <Button variant="ghost" size="icon" onClick={signOut} title="退出登录">
              <LogOut />
            </Button>
          ) : (
            <Button variant="ghost" size="sm" onClick={() => go("/admin/nodes")}>登录</Button>
          )}
        </div>
      </header>

      <main className={`mx-auto space-y-5 px-4 py-6 ${isAdmin ? "max-w-7xl" : "max-w-6xl"}`}>
        {error && <p className="text-sm text-destructive">{error}</p>}

        {!nodes ? (
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {[0, 1, 2].map((i) => <Skeleton key={i} className="h-72" />)}
          </div>
        ) : isAdmin ? (
          <Admin path={path} go={go} nodes={sorted} refresh={refresh} site={location.origin} />
        ) : (
          <>
            <Summary nodes={sorted} />
            {sorted.length === 0 ? (
              <p className="py-16 text-center text-sm text-muted-foreground">
                还没有节点{me.authed ? "，去「进入后台」添加一个" : ""}
              </p>
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
      <Toaster position="top-center" theme={dark ? "dark" : "light"} />
    </div>
  )
}
