/**
 * EvolveRouter adapter for opencode — zero-dependency drop-in.
 *
 * Install: copy this file to  <project>/.opencode/plugins/evolveroute.ts
 * (or ~/.config/opencode/plugins/ for global). opencode loads it at startup.
 *
 * Env: MODELROUTE_URL (default http://127.0.0.1:8787),
 *      MODELROUTE_FEEDBACK=0 to disable outcome reporting.
 *
 * What it does (the gateway does all routing):
 *  - stamps x-ev-session on every LLM call → precise session stickiness
 *  - reports tool success/failure to /api/feedback → flywheel ground truth
 */
const GATEWAY = process.env.MODELROUTE_URL ?? "http://127.0.0.1:8787"
const FEEDBACK_ENABLED = process.env.MODELROUTE_FEEDBACK !== "0"

function classifyToolOutput(title, output) {
  const text = `${title}\n${output}`.toLowerCase()
  const errorMarkers = [
    "error:", "failed", "traceback", "exception", "permission denied",
    "command not found", "no such file", "refused", "panic", "fatal",
    "错误", "失败", "权限被拒绝", "不存在",
  ]
  if (errorMarkers.some((m) => text.includes(m))) {
    return { ok: false, detail: String(output).slice(0, 500) }
  }
  return { ok: true, detail: String(title ?? "").slice(0, 200) }
}

async function report(path, body) {
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

export const ModelroutePlugin = async ({ project }) => {
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
      if (event?.type === "session.error") {
        const props = event.properties ?? {}
        await report("/api/feedback", {
          session: props.sessionID,
          ok: false,
          detail: `session.error: ${JSON.stringify(props).slice(0, 500)}`,
        })
      }
    },
  }
}
