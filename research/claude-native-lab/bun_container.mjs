import assert from "node:assert/strict";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { basename, join } from "node:path";
import { createHash } from "node:crypto";
import { pathToFileURL } from "node:url";

export function inspectContainer(buffer) {
  assert.ok(buffer.length >= 64, "Truncated ELF header");
  assert.equal(buffer.subarray(0, 6).toString("hex"), "7f454c460201", "Expected ELF64 little-endian");
  const sectionTable = Number(buffer.readBigUInt64LE(40));
  const sectionSize = buffer.readUInt16LE(58);
  const sectionCount = buffer.readUInt16LE(60);
  assert.ok(
    sectionSize >= 64 && sectionTable + sectionCount * sectionSize <= buffer.length,
    "Invalid ELF section table",
  );
  assert.ok(buffer.readUInt16LE(62) < sectionCount, "Invalid ELF string section");
  const stringSection = sectionTable + buffer.readUInt16LE(62) * sectionSize;
  const stringOffset = Number(buffer.readBigUInt64LE(stringSection + 24));
  let section;
  for (let index = 0; index < sectionCount; index++) {
    const offset = sectionTable + index * sectionSize;
    const nameStart = stringOffset + buffer.readUInt32LE(offset);
    const name = buffer.subarray(nameStart, buffer.indexOf(0, nameStart)).toString();
    if (name === ".bun")
      section = {
        offset: Number(buffer.readBigUInt64LE(offset + 24)),
        size: Number(buffer.readBigUInt64LE(offset + 32)),
      };
  }
  assert.ok(section, "Missing .bun section");
  assert.ok(section.size >= 128 && section.offset + section.size <= buffer.length, "Invalid .bun section range");
  const dataStart = section.offset + 8;
  const trailer = buffer.indexOf(Buffer.from("\n---- Bun! ----\n"), dataStart + section.size - 128);
  assert.ok(trailer > dataStart && trailer < section.offset + section.size, "Missing Bun trailer");
  const footer = trailer - 32;
  const moduleOffset = buffer.readUInt32LE(footer + 8);
  const moduleLength = buffer.readUInt32LE(footer + 12);
  const entryPoint = buffer.readUInt32LE(footer + 16);
  assert.equal(moduleLength % 52, 0, "Unsupported module layout");
  const checked = (offset, length) => {
    assert.ok(offset >= 0 && length >= 0 && offset + length <= section.size - 8, "Invalid module range");
    return buffer.subarray(dataStart + offset, dataStart + offset + length);
  };
  const modules = [];
  checked(moduleOffset, moduleLength);
  for (let index = 0; index < moduleLength / 52; index++) {
    const record = dataStart + moduleOffset + index * 52;
    const nameOffset = buffer.readUInt32LE(record);
    const nameLength = buffer.readUInt32LE(record + 4);
    assert.ok(nameLength < 4096, "Invalid module name");
    const name = checked(nameOffset, nameLength).toString();
    assert.ok(name.startsWith("/$bunfs/root/") && !name.includes(".."), "Unexpected module path");
    const sourceOffset = buffer.readUInt32LE(record + 8);
    const sourceLength = buffer.readUInt32LE(record + 12);
    const bytecodeOffset = buffer.readUInt32LE(record + 24);
    const bytecodeLength = buffer.readUInt32LE(record + 28);
    checked(sourceOffset, sourceLength);
    checked(bytecodeOffset, bytecodeLength);
    modules.push({ index, name, record, sourceOffset, sourceLength, bytecodeOffset, bytecodeLength });
  }
  return { dataStart, section, entryPoint, modules };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [input, output] = process.argv.slice(2);
  assert.ok(input && output, "Usage: node bun_container.mjs BINARY OUTPUT_DIRECTORY");
  const buffer = readFileSync(input);
  const container = inspectContainer(buffer);
  mkdirSync(output, { recursive: true, mode: 0o700 });
  for (const module of container.modules) {
    if (!module.sourceLength) continue;
    writeFileSync(
      join(output, basename(module.name)),
      buffer.subarray(
        container.dataStart + module.sourceOffset,
        container.dataStart + module.sourceOffset + module.sourceLength,
      ),
      { mode: 0o600, flag: "wx" },
    );
  }
  const sha256 = createHash("sha256").update(buffer).digest("hex");
  writeFileSync(join(output, "manifest.json"), JSON.stringify({ sha256, ...container }, null, 2), {
    mode: 0o600,
    flag: "wx",
  });
  console.log(
    JSON.stringify({ sha256, moduleCount: container.modules.length, entry: container.modules[container.entryPoint] }),
  );
}
