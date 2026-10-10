/**
 * EvolveRouter adapter for opencode.
 *
 * The gateway does all routing; this plugin only enhances it:
 *  - chat.headers: stamp x-ev-session / x-ev-agent so the gateway tracks
 *    sessions precisely (sticky + flywheel keyed by real session id)
 *  - tool.execute.after: report ground-truth tool success/failure to the
 *    gateway (L4 feedback, the highest-precision flywheel signal)
 *  - event: session.error and gateway-side failures are reported back
 *
 * Configuration via environment variables:
 *  EVOLVE_URL   gateway base url (default http://127.0.0.1:8787)
 *  EVOLVE_FEEDBACK  set "0" to disable outcome reporting
 */
import type { Plugin } from "@opencode-ai/plugin"

const GATEWAY = process.env.EVOLVE_URL ?? "http://127.0.0.1:8787"
const FEEDBACK_ENABLED = process.env.EVOLVE_FEEDBACK !== "0"

interface ToolOutcome {
  ok: boolean
  detail?: string
}

/** Best-effort classification of an opencode tool execution result. */
function classifyToolOutput(title: string, output: string): ToolOutcome {
  const text = `${title}\n${output}`.toLowerCase()
  const errorMarkers = [
    "error:", "failed", "traceback", "exception", "permission denied",
    "command not found", "no such file", "refused", "panic", "fatal",
    "错误", "失败", "权限被拒绝", "不存在",
  ]
  if (errorMarkers.some((m) => text.includes(m))) {
    return { ok: false, detail: output.slice(0, 500) }
  }
  return { ok: true, detail: title.slice(0, 200) }
}

async function report(
  path: string,
  body: Record<string, unknown>,
): Promise<void> {
  try {
    await fetch(`${GATEWAY}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(2000),
    })
  } catch {
    // never let observability break the agent
  }
}

export const ModelroutePlugin: Plugin = async ({ project }) => {
  const cwd = project?.worktree ?? project?.path ?? ""
  return {
    "chat.headers": async (input, output) => {
      output.headers["x-ev-session"] = input.sessionID
      if (input.agent) output.headers["x-ev-agent"] = input.agent
      if (cwd) output.headers["x-ev-cwd-len"] = String(cwd.length)
    },

    "tool.execute.after": async (input, output) => {
      if (!FEEDBACK_ENABLED) return
      const outcome = classifyToolOutput(output.title ?? "", output.output ?? "")
      await report("/api/feedback", {
        session: input.sessionID,
        call_id: input.callID,
        tool: input.tool,
        ok: outcome.ok,
        detail: outcome.detail,
      })
    },

    event: async ({ event }) => {
      if (!FEEDBACK_ENABLED) return
      const type = (event as { type?: string }).type
      if (type === "session.error") {
        const props = (event as { properties?: Record<string, unknown> }).properties ?? {}
        await report("/api/feedback", {
          session: (props as { sessionID?: string }).sessionID,
          ok: false,
          detail: `session.error: ${JSON.stringify(props).slice(0, 500)}`,
        })
      }
    },
  }
}
