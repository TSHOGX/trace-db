const { execFileSync } = require("node:child_process");
const { copyFileSync } = require("node:fs");
const { join, resolve } = require("node:path");

const root = resolve(__dirname, "../..");
execFileSync("cargo", ["build", "--locked", "--release", "-p", "tracedb-node"], {
  cwd: root,
  stdio: "inherit",
});

const artifact =
  process.platform === "win32"
    ? "tracedb_node.dll"
    : process.platform === "darwin"
      ? "libtracedb_node.dylib"
      : "libtracedb_node.so";
copyFileSync(join(root, "target", "release", artifact), join(__dirname, "tracedb_node.node"));
