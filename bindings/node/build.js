const { execFileSync } = require("node:child_process");
const { copyFileSync } = require("node:fs");
const { join, resolve } = require("node:path");

const root = resolve(__dirname, "../..");
const target = process.env.TRACEDB_BUILD_TARGET;
const cargoArgs = ["build", "--locked", "--release", "-p", "tracedb-node"];
if (target) {
  cargoArgs.push("--target", target);
}
execFileSync("cargo", cargoArgs, {
  cwd: root,
  stdio: "inherit",
});

const artifact =
  process.platform === "win32"
    ? "tracedb_node.dll"
    : process.platform === "darwin"
      ? "libtracedb_node.dylib"
      : "libtracedb_node.so";
const releaseDir = target
  ? join(root, "target", target, "release")
  : join(root, "target", "release");
copyFileSync(join(releaseDir, artifact), join(__dirname, "tracedb_node.node"));
