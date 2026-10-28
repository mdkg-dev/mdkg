// Test-fixture plugin, separate from mdkg's canonical plugin. No model tool
// except one write to the edit fixture may reach its executor.
// OpenCode 2.1: https://dev.opencode.ai/v2/docs/build/plugins#tools
import { lstatSync, realpathSync, writeFileSync, renameSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const denial = "The hook check permits only its guarded fixture write";

export function safetyPlugin(options) {
  return {
    id: "mdkg-check-safety",
    async setup(ctx) {
      const { directory, session, ready, token } = options;
      if (!isAbsolute(directory) || realpathSync(directory) !== directory ||
          realpathSync(ctx.location.directory) !== directory ||
          dirname(ready) !== dirname(directory) || !/^[a-f0-9]{64}$/.test(token) ||
          typeof session !== "string" || !session) throw new Error("Invalid hook-check fixture");
      const target = join(directory, "guard.txt");
      await ctx.tool.transform((editor) => {
        for (const tool of editor.list()) if (tool.id !== "write") editor.remove(tool.id);
      });
      await ctx.tool.hook("execute.before", (event) => {
        const input = event.input;
        if (event.tool !== "write" || event.sessionID !== session ||
            !input || typeof input !== "object" || Array.isArray(input) ||
            Object.keys(input).some((key) => key !== "path" && key !== "content") ||
            !["guard.txt", "./guard.txt", target].includes(input.path) ||
            typeof input.content !== "string" || input.content.length > 4096) throw new Error(denial);
        // No link, hard link, changed location, or other file can substitute
        // for the sole target, even if the model guesses its absolute path.
        try {
          const stat = lstatSync(target);
          if (!stat.isFile() || stat.nlink !== 1 || realpathSync(target) !== target ||
              realpathSync(directory) !== directory) throw new Error(denial);
        } catch {
          throw new Error(denial);
        }
      });
      const tools = (await ctx.tool.list()).map((tool) => tool.id);
      if (tools.length !== 1 || tools[0] !== "write") throw new Error("Fixture tool restriction did not load");
      // No session starts until the driver verifies this complete receipt.
      writeFileSync(`${ready}.tmp`, JSON.stringify({ token, tools }), { mode: 0o600 });
      renameSync(`${ready}.tmp`, ready);
    },
  };
}

export default {
  id: "mdkg-check-safety",
  async setup(ctx) {
    return safetyPlugin(JSON.parse(process.env.MDKG_CHECK_SAFETY ?? "null")).setup(ctx);
  },
};

async function selfTest() {
  const { default: assert } = await import("node:assert/strict");
  const { mkdtempSync, mkdirSync, readFileSync, rmSync, unlinkSync, symlinkSync, linkSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const root = realpathSync(mkdtempSync(join(tmpdir(), "mdkg-safety-")));
  const directory = join(root, "project");
  mkdirSync(directory);
  const target = join(directory, "guard.txt"), outside = join(root, "external.txt");
  writeFileSync(target, "ORIGINAL");
  writeFileSync(outside, "EXTERNAL");
  let before, transform, executed = 0, checks = 0;
  let catalog = new Map(["write", "read", "shell", "execute", "subagent", "opencode_session_move", "unknown"]
    .map((id) => [id, { id }]));
  const editor = { list: () => [...catalog.values()], remove: (id) => catalog.delete(id) };
  const options = { directory, session: "edit-session", ready: join(root, "ready"), token: "a".repeat(64) };
  try {
    await safetyPlugin(options).setup({ location: { directory }, tool: {
      transform: async (fn) => { transform = fn; fn(editor); },
      hook: async (name, fn) => { assert.equal(name, "execute.before"); before = fn; },
      list: async () => [...catalog.values()],
    } });
    assert.deepEqual([...catalog.keys()], ["write"]);
    assert.deepEqual(JSON.parse(readFileSync(options.ready)), { token: options.token, tools: ["write"] });
    catalog.set("late_mcp_tool", { id: "late_mcp_tool" });
    transform(editor);
    assert.deepEqual([...catalog.keys()], ["write"]);
    checks += 3;
    const invoke = async (tool, input, sessionID = options.session) => {
      await before({ tool, input, sessionID });
      executed++;
      // An executor with a visible outside-fixture side effect, reached
      // only if a forbidden read/shell/unknown call passes the actual gate.
      if (tool !== "write") writeFileSync(outside, "ESCAPED");
      else writeFileSync(resolve(directory, input.path), input.content);
    };
    for (const [tool, input, session] of [
      ["read", { path: outside }], ["shell", { command: "touch external.txt" }],
      ["execute", { code: "read external file" }], ["unknown", {}],
      ["opencode_session_move", { directory: root }], ["Write", { path: "guard.txt", content: "x" }],
      ["write", { path: outside, content: "x" }], ["write", { path: "../external.txt", content: "x" }],
      ["write", { path: "guard.txt", content: "x", filePath: outside }],
      ["write", { path: "guard.txt", content: "x" }, "another-session"],
      ["write", null], ["write", { path: "guard.txt", content: "x".repeat(4097) }],
    ]) {
      await assert.rejects(invoke(tool, input, session), { message: denial });
      checks++;
    }
    assert.equal(executed, 0);
    assert.equal(readFileSync(outside, "utf8"), "EXTERNAL");
    for (const link of [symlinkSync, linkSync]) {
      unlinkSync(target);
      link(outside, target);
      await assert.rejects(invoke("write", { path: "guard.txt", content: "x" }), { message: denial });
      checks++;
    }
    unlinkSync(target);
    await assert.rejects(invoke("write", { path: "guard.txt", content: "x" }), { message: denial });
    checks++;
    writeFileSync(target, "ORIGINAL");
    await invoke("write", { path: "guard.txt", content: "CHANGED" });
    assert.equal(executed, 1);
    assert.equal(readFileSync(target, "utf8"), "CHANGED");
    assert.equal(readFileSync(outside, "utf8"), "EXTERNAL");
    checks++;
    console.log(`OpenCode fixture safety: ${checks} offline checks passed`);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url) &&
    process.argv[2] === "--self-test") await selfTest();
