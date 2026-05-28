#!/usr/bin/env node

const { spawnSync } = require("node:child_process");

const packageByPlatform = {
  "linux:x64": "@jucode/cli-linux-x64",
  "win32:x64": "@jucode/cli-win32-x64"
};

const selector = `${process.platform}:${process.arch}`;
const packageName = packageByPlatform[selector];

if (!packageName) {
  console.error(`Unsupported platform: ${selector}`);
  console.error("Install a release from GitHub Releases for this platform.");
  process.exit(1);
}

const executableName = process.platform === "win32" ? "jucode.exe" : "jucode";

let executablePath;
try {
  executablePath = require.resolve(`${packageName}/bin/${executableName}`);
} catch (error) {
  console.error(`Missing native package ${packageName}.`);
  console.error("Reinstall the package or use a GitHub Release artifact directly.");
  process.exit(1);
}

const result = spawnSync(executablePath, process.argv.slice(2), {
  stdio: "inherit"
});

if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}

if (typeof result.status === "number") {
  process.exit(result.status);
}

process.exit(0);
