export const releaseTargets = [
  {
    directory: "cli-linux-x64",
    packageName: "@jucode/cli-linux-x64",
    rustTarget: "x86_64-unknown-linux-gnu",
    binaryName: "jucode"
  },
  {
    directory: "cli-win32-x64",
    packageName: "@jucode/cli-win32-x64",
    rustTarget: "x86_64-pc-windows-msvc",
    binaryName: "jucode.exe"
  },
  {
    directory: "cli-darwin-arm64",
    packageName: "@jucode/cli-darwin-arm64",
    rustTarget: "aarch64-apple-darwin",
    binaryName: "jucode"
  },
  {
    directory: "cli-darwin-x64",
    packageName: "@jucode/cli-darwin-x64",
    rustTarget: "x86_64-apple-darwin",
    binaryName: "jucode"
  }
];

// Which targets each workflow builds/publishes. macOS lives in its own workflow so
// its slower runners never block the linux/windows release.
export const nativeReleaseTargets = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"];
export const macosReleaseTargets = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

export const rootPackageDirectory = "cli";
export const rootPackageName = "@jucode/cli";
