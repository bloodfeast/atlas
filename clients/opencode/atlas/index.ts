// atlas for OpenCode: forwards OpenCode's hooks to `atlas --hook --client opencode`,
// one process per event, in Claude Code's hook JSON, and applies the answer.
//
// Install: copy this folder to `.opencode/plugins/atlas/` in a project, or to
// `~/.config/opencode/plugins/atlas/` for every project. `atlas` must be on PATH,
// or set ATLAS_BIN to the binary. Add atlas's MCP server too (wiki: Other clients).
//
// Measured on OpenCode v2.4.1: a `context` hook's change reaches the model call
// only (the stored prompt, which the user sees, stays as typed), a `tool`
// `execute.before` throw denies the call and its message reaches the model, and
// `execute.after` may replace the result's content.
import { spawn } from "node:child_process"

const BIN = process.env.ATLAS_BIN || "atlas"
const TIMEOUT_MS = 15_000

// OpenCode's tool names as Claude Code's, for the tools atlas acts on.
const TOOL: Record<string, string> = {
  read: "Read",
  shell: "Bash",
  bash: "Bash",
  grep: "Grep",
  glob: "Glob",
  edit: "Edit",
  write: "Write",
  // `task` keeps its name: atlas's agent caps count Claude Code's launches only.
}

type Answer = Record<string, any> | string | null

// Runs one hook event; the answer as JSON, as plain text (PreCompact's
// instructions), or null for nothing or any failure (a hook fails open). It
// never blocks: OpenCode's server runs every session's hooks.
function hook(event: Record<string, unknown>): Promise<Answer> {
  return new Promise((resolve) => {
    let out = ""
    let done = false
    const finish = (a: Answer) => {
      if (!done) {
        done = true
        resolve(a)
      }
    }
    try {
      const p = spawn(BIN, ["--hook", "--client", "opencode"], { env: process.env, stdio: ["pipe", "pipe", "ignore"] })
      const timer = setTimeout(() => {
        p.kill()
        finish(null)
      }, TIMEOUT_MS)
      p.stdout.setEncoding("utf8")
      p.stdout.on("data", (d: string) => (out += d))
      p.on("error", () => finish(null))
      p.on("close", (code: number | null) => {
        clearTimeout(timer)
        const t = out.trim()
        if (code !== 0 || !t) return finish(null)
        try {
          finish(JSON.parse(t))
        } catch {
          finish(t)
        }
      })
      p.stdin.on("error", () => {})
      p.stdin.end(JSON.stringify(event))
    } catch {
      finish(null)
    }
  })
}

function specific(a: Answer): Record<string, any> {
  return a && typeof a === "object" ? a.hookSpecificOutput || {} : {}
}

function text(content: unknown): string {
  return Array.isArray(content) ? content.map((c: any) => (c?.type === "text" ? c.text : "")).join("") : ""
}

// The tool's input in Claude Code's shape.
function claudeInput(tool: string, input: any): Record<string, unknown> {
  const i = { ...(input || {}) }
  if (tool === "Read") i.file_path = i.path ?? i.filePath
  return i
}

// The tool's result in Claude Code's shape, for the tools whose results atlas digests.
function claudeResponse(tool: string, input: any, result: any): Record<string, unknown> {
  const out = result?.output
  if (tool === "Bash") {
    const stdout = typeof out?.output === "string" ? out.output : text(result?.content)
    const r: Record<string, unknown> = { stdout, stderr: "", interrupted: false, isImage: false, noOutputExpected: false }
    // A long output is cut to its last 2,000 lines; the shell tool keeps the
    // whole of it in `outputPath`, as Claude Code's `persistedOutputPath` does.
    if (result?.metadata?.truncated && typeof out?.outputPath === "string") r.persistedOutputPath = out.outputPath
    return r
  }
  if (tool === "Read" && out?.type === "text") {
    const lines = Number(out.lineCount) || 0
    return { type: "text", file: { filePath: input.file_path, content: out.content, numLines: lines, startLine: Number(out.offset) || 1, totalLines: lines } }
  }
  return { text: text(result?.content) }
}

// The text of a replacement output atlas wrote in a Claude tool's shape.
function replacement(tool: string, u: any): string | null {
  if (typeof u === "string") return u
  if (tool === "Bash" && typeof u?.stdout === "string") return u.stdout
  if (tool === "Read" && typeof u?.file?.content === "string") return u.file.content
  return null
}

export default {
  id: "atlas",
  async setup(ctx: any) {
    const cwd: string = ctx.location?.directory || process.cwd()
    const started = new Set<string>()
    // atlas's notes for a prompt, by its message id. They join that message in
    // every model call (the `context` hook), never the stored prompt: OpenCode
    // shows the stored one as the user's own words.
    const notesFor = new Map<string, string>()
    const KEEP = 500
    // Context a tool hook gave before the tool ran: it goes out with the result.
    const pending = new Map<string, string[]>()
    const base = (sessionID: string, event: string) => ({ hook_event_name: event, session_id: sessionID, cwd })

    await ctx.session.hook("prompt", async (e: any) => {
      const notes: string[] = []
      if (!started.has(e.sessionID)) {
        started.add(e.sessionID)
        const c = specific(await hook({ ...base(e.sessionID, "SessionStart"), source: "startup" })).additionalContext
        if (c) notes.push(c)
      }
      const c = specific(await hook({ ...base(e.sessionID, "UserPromptSubmit"), prompt: e.prompt?.text || "" })).additionalContext
      if (c) notes.push(c)
      if (!notes.length) return
      notesFor.set(e.messageID, `<atlas>\n${notes.join("\n")}\n</atlas>`)
      if (notesFor.size > KEEP) notesFor.delete(notesFor.keys().next().value as string)
    })

    await ctx.session.hook("context", (e: any) => {
      for (const m of e.messages || []) {
        const n = m.role === "user" ? notesFor.get(m.id) : undefined
        if (n && Array.isArray(m.content)) m.content.push({ type: "text", text: n })
      }
    })

    await ctx.tool.hook("execute.before", async (e: any) => {
      const tool = TOOL[e.tool] || e.tool
      const a = await hook({ ...base(e.sessionID, "PreToolUse"), tool_name: tool, tool_use_id: e.id, tool_input: claudeInput(tool, e.input) })
      const h = specific(a)
      if (h.permissionDecision === "deny") throw new Error(h.permissionDecisionReason || "denied by atlas")
      // atlas's Bash rewrite only appends an exit-code marker, which OpenCode
      // reports itself: the command runs as the model wrote it.
      if (h.additionalContext) pending.set(e.id, [...(pending.get(e.id) || []), h.additionalContext])
    })

    await ctx.tool.hook("execute.after", async (e: any) => {
      const notes = pending.get(e.id) || []
      pending.delete(e.id)
      if (e.status !== "completed") return
      const tool = TOOL[e.tool] || e.tool
      const input = claudeInput(tool, e.input)
      const a = await hook({ ...base(e.sessionID, "PostToolUse"), tool_name: tool, tool_use_id: e.id, tool_input: input, tool_response: claudeResponse(tool, input, e.result) })
      const h = specific(a)
      let content = e.result?.content
      const t = h.updatedToolOutput !== undefined ? replacement(tool, h.updatedToolOutput) : null
      if (t !== null) content = [{ type: "text", text: t }]
      if (h.additionalContext) notes.push(h.additionalContext)
      if (notes.length) content = [...(Array.isArray(content) ? content : []), { type: "text", text: notes.join("\n") }]
      if (content !== e.result?.content) e.result = { ...e.result, content }
    })

    await ctx.session.hook("compaction", async (e: any) => {
      const a = await hook({ ...base(e.sessionID, "PreCompact"), trigger: "auto" })
      // PreCompact's plain text is instructions for the summary.
      if (typeof a === "string" && a && Array.isArray(e.system)) e.system.push({ type: "text", text: a })
    })
  },
}
