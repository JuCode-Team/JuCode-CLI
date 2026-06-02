import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, statSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { releaseTargets, rootPackageDirectory } from "./release-targets.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..", "..");
const outputDirectory = path.join(repoRoot, "dist", "npm");
const args = process.argv.slice(2);
const requestedTargets = new Set();

for (let index = 0; index < args.length; index += 1) {
  const argument = args[index];
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

const selectedTargets =
  requestedTargets.size === 0
    ? releaseTargets
    : releaseTargets.filter((target) => requestedTargets.has(target.rustTarget));

if (selectedTargets.length === 0) {
  throw new Error("No release targets matched the requested filters");
}

mkdirSync(outputDirectory, { recursive: true });

for (const target of selectedTargets) {
  const binaryPath = path.join(repoRoot, "npm", target.directory, "bin", target.binaryName);
  if (!existsSync(binaryPath)) {
    throw new Error(`Missing staged binary for ${target.rustTarget}: ${binaryPath}`);
  }

  if (target.binaryName !== "jucode.exe" && (statSync(binaryPath).mode & 0o111) === 0) {
    throw new Error(`Staged binary is not executable for ${target.rustTarget}: ${binaryPath}`);
  }
}

for (const directory of [...selectedTargets.map((target) => target.directory), rootPackageDirectory]) {
  const cwd = path.join(repoRoot, "npm", directory);
  if (process.platform === "win32") {
    execFileSync(
      process.env.comspec || "cmd.exe",
      ["/d", "/s", "/c", "npm", "pack", "--pack-destination", outputDirectory],
      {
        cwd,
        stdio: "inherit"
      }
    );
    continue;
  }

  execFileSync("npm", ["pack", "--pack-destination", outputDirectory], {
    cwd,
    stdio: "inherit"
  });
}
