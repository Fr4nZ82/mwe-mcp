/**
 * mwe-mcp bridge — the switch, and nothing else.
 *
 * Deliberately its own module with no import but `fs`: the memory scaffold and
 * the session-start hook ask this question at boot, and one of them runs as a
 * standalone script under Claude Code. Neither should have to load the mailbox
 * to find out whether the memory is on.
 */
import fs from 'fs';

/**
 * A group is an mwe agent when the `mwe` plugin is stamped into it. The
 * persona and the mechanics arrive together, from the same template, so there
 * is no second setting that can disagree with the first.
 */
const PLUGIN_MANIFEST = '/workspace/agent/plugins/mwe/plugin.json';

let active: boolean | undefined;

export function mweActive(): boolean {
  if (active === undefined) {
    active = fs.existsSync(process.env.NANOCLAW_MWE_PLUGIN_MANIFEST || PLUGIN_MANIFEST);
    console.error(`[mwe] ${active ? 'memory is on for this agent' : 'no mwe plugin in this group — memory is off'}`);
  }
  return active;
}

/**
 * The sessionless model: with memory on, no `continuation` is ever carried
 * from one turn to the next. Each turn is a fresh provider query whose
 * conversational context is the bridge's window plus the recall block, and no
 * compaction or summarization pass ever runs — recall replaces the summary.
 */
export function mweStateless(): boolean {
  return mweActive();
}
