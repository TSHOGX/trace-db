import { copyFileSync, mkdirSync } from "fs";
import { join } from "path";

const build = Bun.spawn(["bun", "build", "--compile", "src/cli.ts", "--outfile", "trace-db-bin"], {
  stdout: "inherit",
  stderr: "inherit",
});
const code = await build.exited;
if (code !== 0) process.exit(code);

const extension = process.platform === "darwin" ? "dylib" : process.platform === "win32" ? "dll" : "so";
const filename = process.platform === "win32" ? "fts5jieba.dll" : `libfts5jieba.${extension}`;
const source = join("native", "fts5-jieba", "target", "release", filename);
const destination = join("native", filename);
mkdirSync("native", { recursive: true });
copyFileSync(source, destination);
