import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { releaseTargets, rootPackageDirectory } from "./release-targets.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(__dirname, "..", "..");

const cargoToml = readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8");
const versionMatch = cargoToml.match(/^\s*version\s*=\s*"([^"]+)"/m);

if (!versionMatch) {
  throw new Error("Unable to read version from Cargo.toml");
}

const cargoVersion = versionMatch[1];
const requestedVersion = process.env.RELEASE_VERSION?.replace(/^v/, "");

if (requestedVersion && requestedVersion !== cargoVersion) {
  throw new Error(
    `Release version ${requestedVersion} does not match Cargo.toml version ${cargoVersion}`
  );
}

const packageDirectories = [rootPackageDirectory, ...releaseTargets.map((target) => target.directory)];

for (const directory of packageDirectories) {
  const packageJsonPath = path.join(repoRoot, "npm", directory, "package.json");
  const packageJson = JSON.parse(readFileSync(packageJsonPath, "utf8"));
  packageJson.version = cargoVersion;

  if (directory === rootPackageDirectory) {
    packageJson.optionalDependencies = {};
    for (const target of releaseTargets) {
      packageJson.optionalDependencies[target.packageName] = cargoVersion;
    }
  }

  writeFileSync(packageJsonPath, `${JSON.stringify(packageJson, null, 2)}\n`);
}

console.log(`Synchronized npm package versions to ${cargoVersion}`);
