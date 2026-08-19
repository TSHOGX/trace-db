"use strict";

const { TraceDb: NativeTraceDb } = require("./tracedb_node.node");

class TraceDb {
  constructor(native) {
    this._native = native;
  }

  static open(path) {
    return new TraceDb(NativeTraceDb.open(path));
  }

  statsJson() {
    return this._native.statsJson();
  }

  stats() {
    return JSON.parse(this.statsJson());
  }

  searchJson(query, limit = 20, agent, cwd, sinceMs) {
    return this._native.searchJson(query, limit, agent, cwd, sinceMs);
  }

  search(query, options = {}) {
    const { limit = 20, agent, cwd, sinceMs } = options;
    return JSON.parse(this.searchJson(query, limit, agent, cwd, sinceMs));
  }

  ingestJson(agents, mode = "partial", root, sinceMs) {
    return this._native.ingestJson(agents, mode, root, sinceMs);
  }

  ingest(options = {}) {
    const { agents, mode = "partial", root, sinceMs } = options;
    return JSON.parse(this.ingestJson(agents, mode, root, sinceMs));
  }

  showJson(sessionId) {
    return this._native.showJson(sessionId);
  }

  show(sessionId) {
    return JSON.parse(this.showJson(sessionId));
  }

  reconstructJson(sessionId, outDir) {
    return this._native.reconstructJson(sessionId, outDir);
  }

  reconstruct(sessionId, outDir) {
    return JSON.parse(this.reconstructJson(sessionId, outDir));
  }

  reindex() {
    return this._native.reindex();
  }
}

module.exports = { TraceDb };
