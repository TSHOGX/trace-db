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

  coverageJson(sessionId) {
    return this._native.coverageJson(sessionId);
  }

  coverage(sessionId) {
    return JSON.parse(this.coverageJson(sessionId));
  }

  searchJson(query, limit = 20, agent, cwd, sinceMs) {
    return this._native.searchJson(query, limit, agent, cwd, sinceMs);
  }

  search(query, options = {}) {
    const { limit = 20, agent, cwd, sinceMs } = options;
    return JSON.parse(this.searchJson(query, limit, agent, cwd, sinceMs));
  }

  listJson(
    limit = 50,
    cursor,
    agent,
    cwd,
    sinceMs,
    model,
    provider,
    cwdExact = false,
    collapseLineage = false,
  ) {
    return this._native.listJson(
      limit,
      cursor,
      agent,
      cwd,
      sinceMs,
      model,
      provider,
      cwdExact,
      collapseLineage,
    );
  }

  list(options = {}) {
    const {
      limit = 50,
      cursor,
      agent,
      cwd,
      sinceMs,
      model,
      provider,
      cwdExact = false,
      collapseLineage = false,
    } = options;
    return JSON.parse(
      this.listJson(
        limit,
        cursor,
        agent,
        cwd,
        sinceMs,
        model,
        provider,
        cwdExact,
        collapseLineage,
      ),
    );
  }

  ingestJson(agents, root, sinceMs) {
    return this._native.ingestJson(agents, root, sinceMs);
  }

  ingest(options = {}) {
    const { agents, root, sinceMs } = options;
    return JSON.parse(this.ingestJson(agents, root, sinceMs));
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

  reconstructJsonWithOptions(sessionId, outDir, overwrite = false) {
    return this._native.reconstructJsonWithOptions(
      sessionId,
      outDir,
      overwrite,
    );
  }

  reconstruct(sessionId, outDir, overwrite = false) {
    return JSON.parse(
      this.reconstructJsonWithOptions(sessionId, outDir, overwrite),
    );
  }

  reindex() {
    return this._native.reindex();
  }
}

module.exports = { TraceDb };
