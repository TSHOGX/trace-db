/**
 * errors.ts — typed errors mapped to exit codes in sess.ts runCli().
 * Mirrors the sibling CLIs (nw/rss/xhs/x/pod).
 *
 * Exit codes:
 *   0  success
 *   1  generic error
 *   2  not-found (unknown session / missing store)
 *   4  usage error (bad args)
 */

export class SessError extends Error {
  constructor(
    public code: number,
    message: string
  ) {
    super(message);
    this.name = "SessError";
  }
}

/** Unknown session id, or a native store that isn't present. */
export class NotFoundError extends SessError {
  constructor(message: string) {
    super(2, message);
    this.name = "NotFoundError";
  }
}

/** Bad CLI arguments. */
export class UsageError extends SessError {
  constructor(message: string) {
    super(4, message);
    this.name = "UsageError";
  }
}
