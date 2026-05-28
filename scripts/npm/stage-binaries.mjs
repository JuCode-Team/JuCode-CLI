import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { releaseTargets } from "./release-targets.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..", "..");

const args = process.argv.slice(2);
let sourceRoot = path.join(repoRoot, "artifacts");
const requestedTargets = new Set();

for (let index = 0; index < args.length; index += 1) {
  const argument = args[index];
  if (argument === "--source-root") {
    const next = args[index + 1];
    if (!next) {
      throw new Error("--source-root requires a value");
    }
    sourceRoot = path.resolve(repoRoot, next);
    index += 1;
    continue;
  }

  if (argument === "--target") {
    const next = args[index + 1];
    if (!next) {
      throw new Error("--target requires a value");
    }
    requestedTargets.add(next);
    index += 1;
    continue;
  }

  throw new Error(`Unknown argument: ${argument}`);
}

const targets =
  requestedTargets.size === 0
    ? releaseTargets
    : releaseTargets.filter((target) => requestedTargets.has(target.rustTarget));

if (targets.length === 0) {
  throw new Error("No release targets matched the requested filters");
}

for (const target of targets) {
  const sourcePath = path.join(sourceRoot, target.rustTarget, target.binaryName);
  if (!existsSync(sourcePath)) {
    throw new Error(`Missing binary for ${target.rustTarget}: ${sourcePath}`);
  }

  const destinationDirectory = path.join(repoRoot, "npm", target.directory, "bin");
  mkdirSync(destinationDirectory, { recursive: true });

  const destinationPath = path.join(destinationDirectory, target.binaryName);
  copyFileSync(sourcePath, destinationPath);
  console.log(`Staged ${target.rustTarget} -> npm/${target.directory}/bin/${target.binaryName}`);
}
