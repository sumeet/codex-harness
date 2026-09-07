import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { pathToFileURL } from "node:url";
import { inspectContainer } from "./bun_container.mjs";

export const verifiedBuilds = new Set(["b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca"]);

export function registrySpecification(modules) {
  const candidates = modules.filter(
    ({ source }) =>
      source.length < 10000 &&
      source.includes("extends Map") &&
      source.includes("claimForStandaloneRender") &&
      source.includes("pendingStandaloneRender"),
  );
  if (candidates.length !== 1) throw new Error(`Expected one native Ink registry, found ${candidates.length}`);
  const { name, source } = candidates[0];
  const exports = [];
  for (const match of source.matchAll(/export\s*\{([^}]+)\}/g)) {
    for (const item of match[1].split(",")) {
      const parsed = /^\s*([\w$]+)(?:\s+as\s+([\w$]+))?\s*$/.exec(item);
      if (!parsed) throw new Error("Unrecognized native registry export syntax");
      exports.push({ local: parsed[1], exported: parsed[2] ?? parsed[1] });
    }
  }
  const getters = [
    ...source.matchAll(/function\s+([\w$]+)\(\)\s*\{\s*return\s+[\w$]+\.of\([\w$]+\(\)\.host\);?\s*\}/g),
  ];
  if (getters.length === 1) {
    const names = exports.filter((value) => value.local === getters[0][1]);
    if (names.length !== 1) throw new Error("Native registry getter is not uniquely exported");
    return { registryModule: name, registryExport: names[0].exported, registryKind: "host-map-getter" };
  }
  if (getters.length !== 0 || exports.length === 0) throw new Error("Ambiguous native registry getter");
  return { registryModule: name, registryKind: "map", registryExports: exports.map((value) => value.exported) };
}

export function discoverBinary(path) {
  const buffer = readFileSync(path);
  const container = inspectContainer(buffer);
  const modules = container.modules
    .filter((module) => module.sourceLength < 10000)
    .map((module) => ({
      name: module.name,
      source: buffer
        .subarray(
          container.dataStart + module.sourceOffset,
          container.dataStart + module.sourceOffset + module.sourceLength,
        )
        .toString(),
    }));
  const sha256 = createHash("sha256").update(buffer).digest("hex");
  return { sha256, verified: verifiedBuilds.has(sha256), ...registrySpecification(modules),
    ...(sha256 === "b9c407e36847bcb24b953b1390f240c840ae6c99e10a76475d2fadc5d5c4adca" ? {
      settingsControls: {
        modelModule: "/$bunfs/root/chunk-fhqmwd1d.js",
        effortModule: "/$bunfs/root/chunk-0w8exbe0.js",
        permissionModule: "/$bunfs/root/chunk-7kc4t68e.js",
        validatePermissionExport: "K$e", setPermissionExport: "iI",
      },
    } : {}),
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.argv.length !== 3) throw new Error("Usage: node discover.mjs EXACT_NATIVE_CLAUDE_BINARY");
  console.log(JSON.stringify(discoverBinary(process.argv[2]), null, 2));
}
