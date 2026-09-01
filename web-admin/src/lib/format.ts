const UNITS = ["B", "KB", "MB", "GB", "TB", "PB"]

const unitOf = (n: number) => Math.min(Math.floor(Math.log(n) / Math.log(1024)), UNITS.length - 1)

/**
 * 1024-based, as every VPS dashboard and `df` report bytes, but labelled MB/GB
 * the way `df -h` and every hosting plan write it. Three significant digits by
 * default. Kept in step with the theme's copy of this file.
 */
export function bytes(n: number, digits?: number): string {
  // `< 1`, not `< 0`: a fraction of a byte lands `unitOf` on -1 and prints
  // "512 undefined".
  if (!n || n < 1) return "0 B"
  const i = unitOf(n)
  const v = n / 1024 ** i
  return `${v.toFixed(i === 0 ? 0 : (digits ?? (v >= 100 ? 0 : v >= 10 ? 1 : 2)))} ${UNITS[i]}`
}

export function uptime(seconds: number): string {
  if (!seconds) return "—"
  const d = Math.floor(seconds / 86400)
  const h = Math.floor((seconds % 86400) / 3600)
  const m = Math.floor((seconds % 3600) / 60)
  return d > 0 ? `${d} 天 ${h} 小时` : h > 0 ? `${h} 小时 ${m} 分` : `${m} 分`
}

/**
 * No expiry, no traffic cap: the same "there is no ceiling here" either way.
 * U+221E rather than ♾️, which arrives as a coloured tile from whatever font
 * the browser has. This one inherits the text colour and size.
 */
export const FOREVER = "∞"

const SYMBOLS: Record<string, string> = { USD: "$", CNY: "¥", EUR: "€", GBP: "£", JPY: "¥" }

export function money(amount: number, currency: string): string {
  return `${SYMBOLS[currency] ?? ""}${amount.toFixed(2)}${SYMBOLS[currency] ? "" : ` ${currency}`}`
}

export const CYCLES: Record<string, string> = {
  monthly: "月付",
  quarterly: "季付",
  semiannual: "半年付",
  yearly: "年付",
  biennial: "两年付",
  triennial: "三年付",
  once: "一次性",
}

/**
 * Usage counted the way the plan bills it: summing both directions regardless
 * measures a node billed on upload alone against the wrong number.
 */
export function monthUsage(node: { month_rx: number; month_tx: number; traffic_mode: string }): number {
  switch (node.traffic_mode) {
    case "up":
      return node.month_tx
    case "down":
      return node.month_rx
    case "max":
      return Math.max(node.month_rx, node.month_tx)
    default:
      return node.month_rx + node.month_tx
  }
}
