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
  }
];

export const rootPackageDirectory = "cli";
export const rootPackageName = "@jucode/cli";
