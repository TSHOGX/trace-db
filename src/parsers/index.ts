/**
 * parsers/index.ts — the parser registry. The one place agent → Parser is
 * resolved. ingest.ts and sess.ts dispatch through this and never import a
 * concrete parser directly, so adding an agent is: write parsers/<x>.ts,
 * register it here.
 */

import type { Agent, Parser } from "../types.js";
import { claudeParser } from "./claude.js";
import { codexParser } from "./codex.js";
import { geminiParser } from "./gemini.js";
import { opencodeParser } from "./opencode.js";
import { piParser } from "./pi.js";

export const PARSERS: Record<Agent, Parser> = {
  claude: claudeParser,
  codex: codexParser,
  opencode: opencodeParser,
  gemini: geminiParser,
  pi: piParser,
};

export function getParser(agent: Agent): Parser {
  return PARSERS[agent];
}
