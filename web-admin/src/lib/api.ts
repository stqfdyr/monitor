import { useEffect, useState } from "react"

export type Metrics = {
  uptime: number
  cpu: number
  load: [number, number, number]
  mem_total: number
  mem_used: number
  swap_total: number
  swap_used: number
  disk_total: number
  disk_used: number
  net_rx: number
  net_tx: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  tcp: number
  udp: number
  procs: number
}

export type Node = {
  id: number
  name: string
  sort: number
  public: boolean
  online: boolean
  last_seen: number
  metrics: Metrics | null
  os: string
  kernel: string
  arch: string
  virt: string
  cpu_name: string
  cpu_cores: number
  mem_total: number
  swap_total: number
  disk_total: number
  agent_version: string
  price: number
  currency: string
  billing_cycle: string
  expires_at: string | null
  traffic_limit: number
  traffic_mode: string
  traffic_reset_day: number
  total_rx: number
  total_tx: number
  month_rx: number
  month_tx: number
  month_start: string
  /** Panel only. */
  hostname?: string
  ip?: string
  ipv4?: string
  ipv6?: string
  remark?: string
}

export type PingTask = { id: number; name: string; target: string; interval: number; nodes: number[] }

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  })
  if (!res.ok) throw new ApiError(res.status, (await res.text()) || res.statusText)
  return res.status === 204 ? (undefined as T) : res.json()
}

/**
 * Live node list. Uses the WebSocket the hub pushes every two seconds and
 * falls back to polling if it cannot be established.
 */
export function useNodes() {
  const [nodes, setNodes] = useState<Node[] | null>(null)
  const [admin, setAdmin] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)

  useEffect(() => {
    let socket: WebSocket | null = null
    let poll: ReturnType<typeof setInterval> | null = null
    let closed = false

    const fetchOnce = () =>
      api<{ nodes: Node[]; admin: boolean }>("/nodes")
        .then((d) => {
          setNodes(d.nodes)
          setAdmin(d.admin)
          setError(null)
        })
        .catch((e: Error) => setError(e.message))

    fetchOnce()

    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/api/ws`
    try {
      socket = new WebSocket(url)
      socket.onmessage = (event) => setNodes(JSON.parse(event.data).nodes)
      socket.onerror = () => socket?.close()
      socket.onclose = () => {
        if (!closed && !poll) poll = setInterval(fetchOnce, 5000)
      }
    } catch {
      poll = setInterval(fetchOnce, 5000)
    }

    return () => {
      closed = true
      socket?.close()
      if (poll) clearInterval(poll)
    }
  }, [reload])

  return { nodes, admin, error, refresh: () => setReload((n) => n + 1) }
}
