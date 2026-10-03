// Packs the TypeScript SDK workspace packages into release tarballs.
//
// usage: bun sdk/ts/scripts/pack-release.ts --version X.Y.Z --base-url URL --out DIR
//
// Writes `pohunek-ts-<package>-X.Y.Z.tgz` and `<tarball>.sha256` for each of
// `protocol`, `sdk` and `testkit` into DIR. Every `@pohunek/*` dependency of a
// packed manifest is rewritten to the exact URL of the sibling tarball under
// `--base-url`, so a consumer that pins one tarball URL receives the whole
// closure from the same release and never asks a package registry for the
// `@pohunek` scope. `devDependencies`, `scripts` and `private` are dropped from
// the packed manifest and `version` is set.
//
// Tarballs are byte-reproducible for one host zlib: entries are sorted in byte
// order, owned by root, stamped with `SOURCE_DATE_EPOCH` (required, so an
// archive is never stamped with the build time), and written with fixed modes.
// The gzip stream carries no name and a zero timestamp.

import { createHash } from "node:crypto";
import { access, mkdir, readdir, readFile, writeFile } from "node:fs/promises";
import { basename, dirname, join, posix } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPOSITORY_ROOT = join(SCRIPT_DIR, "..", "..", "..");
const SCOPE_PREFIX = "@pohunek/";
const ASSET_PREFIX = "pohunek-ts-";
const ASSET_SUFFIX = ".tgz";
// Matches the repository LICENSE file shipped inside every tarball.
const LICENSE_IDENTIFIER = "MIT";
const LICENSE_FILE = "LICENSE";
const README_FILE = "README.md";
// Directories whose full contents are published when present in a package.
const PUBLISHED_DIRECTORIES = ["src", "fixtures"] as const;
// npm unpacks a tarball into the single top-level directory `package/`.
const TARBALL_ROOT = "package";
const DEPENDENCY_FIELDS = ["dependencies", "peerDependencies", "optionalDependencies"] as const;
const DROPPED_FIELDS = new Set(["private", "devDependencies", "scripts", "version"]);
const VERSION_PATTERN = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?$/u;

// ustar header geometry (POSIX.1-1988).
const TAR_BLOCK = 512;
const TAR_NAME_MAX = 100;
const TAR_PREFIX_MAX = 155;
const TAR_FILE_MODE = 0o644;
const GZIP_OS_BYTE_OFFSET = 9;
// RFC 1952 OS value for Unix; zlib would otherwise record the build host.
const GZIP_OS_UNIX = 3;

export interface PackedPackage {
  readonly name: string;
  readonly tarball: string;
  readonly checksum: string;
  readonly sha256: string;
  readonly files: readonly string[];
}

export interface PackReleaseOptions {
  readonly version: string;
  readonly baseUrl: string;
  readonly outDir: string;
  readonly sourceDateEpoch: number;
}

interface PackageSpec {
  readonly directory: string;
  readonly name: string;
  readonly asset: string;
}

type JsonObject = Record<string, unknown>;

const PACKAGE_DIRECTORIES = ["protocol", "sdk", "testkit"] as const;

function assetName(directory: string, version: string): string {
  return `${ASSET_PREFIX}${directory}-${version}${ASSET_SUFFIX}`;
}

function isRecord(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validateOptions(options: PackReleaseOptions): string {
  if (!VERSION_PATTERN.test(options.version)) {
    throw new Error(`version must be a semantic version without a leading "v": ${options.version}`);
  }
  let url: URL;
  try {
    url = new URL(options.baseUrl);
  } catch {
    throw new Error(`base URL is not an absolute URL: ${options.baseUrl}`);
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    throw new Error(`base URL must use http or https: ${options.baseUrl}`);
  }
  if (url.search !== "" || url.hash !== "" || url.username !== "" || url.password !== "") {
    throw new Error(`base URL must not carry credentials, a query or a fragment: ${options.baseUrl}`);
  }
  if (!Number.isSafeInteger(options.sourceDateEpoch) || options.sourceDateEpoch < 0) {
    throw new Error(`SOURCE_DATE_EPOCH must be a non-negative integer: ${options.sourceDateEpoch}`);
  }
  if (options.outDir === "") {
    throw new Error("output directory must not be empty");
  }
  return options.baseUrl.replace(/\/+$/u, "");
}

async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

async function collectFiles(root: string, relative: string, into: string[]): Promise<void> {
  const entries = await readdir(join(root, relative), { withFileTypes: true });
  for (const entry of entries) {
    const child = posix.join(relative, entry.name);
    if (entry.isDirectory()) {
      await collectFiles(root, child, into);
    } else if (entry.isFile()) {
      into.push(child);
    } else {
      throw new Error(`refusing to pack a non-regular file: ${join(root, child)}`);
    }
  }
}

function byteOrder(left: string, right: string): number {
  return Buffer.compare(Buffer.from(left), Buffer.from(right));
}

function exportTargets(value: unknown, into: string[]): void {
  if (typeof value === "string") {
    into.push(value);
  } else if (isRecord(value)) {
    for (const child of Object.values(value)) {
      exportTargets(child, into);
    }
  }
}

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&");
}

// Every path the `exports` and `types` fields reference must be in the tarball;
// a missing target would only surface in a consumer's import.
function assertExportsPacked(manifest: JsonObject, files: readonly string[], label: string): void {
  const targets: string[] = [];
  exportTargets(manifest["exports"], targets);
  if (typeof manifest["types"] === "string") {
    targets.push(manifest["types"]);
  }
  for (const target of targets) {
    const path = target.replace(/^\.\//u, "");
    const matcher = new RegExp(`^${path.split("*").map(escapeRegExp).join(".+")}$`, "u");
    if (!files.some((file) => matcher.test(file))) {
      throw new Error(`${label}: export target ${target} is not part of the tarball`);
    }
  }
}

function rewriteDependencies(
  manifest: JsonObject,
  specs: readonly PackageSpec[],
  baseUrl: string,
  label: string,
): JsonObject {
  const result: JsonObject = {};
  for (const [key, value] of Object.entries(manifest)) {
    if (DROPPED_FIELDS.has(key)) {
      continue;
    }
    if ((DEPENDENCY_FIELDS as readonly string[]).includes(key)) {
      if (!isRecord(value)) {
        throw new Error(`${label}: ${key} must be an object`);
      }
      const rewritten: Record<string, string> = {};
      for (const [dependency, range] of Object.entries(value)) {
        if (typeof range !== "string") {
          throw new Error(`${label}: ${key}.${dependency} must be a string`);
        }
        if (dependency.startsWith(SCOPE_PREFIX)) {
          const sibling = specs.find((spec) => spec.name === dependency);
          if (sibling === undefined) {
            throw new Error(`${label}: ${key}.${dependency} is not a packed workspace package`);
          }
          rewritten[dependency] = `${baseUrl}/${sibling.asset}`;
        } else if (range.startsWith("workspace:")) {
          throw new Error(`${label}: ${key}.${dependency} is a workspace dependency outside ${SCOPE_PREFIX}`);
        } else {
          rewritten[dependency] = range;
        }
      }
      result[key] = rewritten;
    } else {
      result[key] = value;
    }
  }
  return result;
}

function octal(value: number, width: number): string {
  return `${value.toString(8).padStart(width - 1, "0")}\0`;
}

function splitTarPath(path: string): { name: string; prefix: string } {
  if (Buffer.byteLength(path) <= TAR_NAME_MAX) {
    return { name: path, prefix: "" };
  }
  for (let index = path.indexOf("/"); index !== -1; index = path.indexOf("/", index + 1)) {
    const prefix = path.slice(0, index);
    const name = path.slice(index + 1);
    if (Buffer.byteLength(prefix) <= TAR_PREFIX_MAX && Buffer.byteLength(name) <= TAR_NAME_MAX) {
      return { name, prefix };
    }
  }
  throw new Error(`path too long for a ustar header: ${path}`);
}

function tarEntry(path: string, content: Buffer, mtime: number): Buffer {
  const { name, prefix } = splitTarPath(path);
  const header = Buffer.alloc(TAR_BLOCK);
  header.write(name, 0, "utf8");
  header.write(octal(TAR_FILE_MODE, 8), 100, "ascii");
  header.write(octal(0, 8), 108, "ascii");
  header.write(octal(0, 8), 116, "ascii");
  header.write(octal(content.length, 12), 124, "ascii");
  header.write(octal(mtime, 12), 136, "ascii");
  header.fill(0x20, 148, 156);
  header.write("0", 156, "ascii");
  header.write("ustar\0", 257, "ascii");
  header.write("00", 263, "ascii");
  header.write(prefix, 345, "utf8");
  let checksum = 0;
  for (const byte of header) {
    checksum += byte;
  }
  header.write(`${checksum.toString(8).padStart(6, "0")}\0 `, 148, "ascii");
  const padding = Buffer.alloc((TAR_BLOCK - (content.length % TAR_BLOCK)) % TAR_BLOCK);
  return Buffer.concat([header, content, padding]);
}

function buildTarball(entries: ReadonlyMap<string, Buffer>, mtime: number): Buffer {
  const paths = [...entries.keys()].sort(byteOrder);
  const blocks = paths.map((path) => tarEntry(`${TARBALL_ROOT}/${path}`, entries.get(path) as Buffer, mtime));
  blocks.push(Buffer.alloc(TAR_BLOCK * 2));
  const gzip = gzipSync(Buffer.concat(blocks), { level: 9 });
  gzip[GZIP_OS_BYTE_OFFSET] = GZIP_OS_UNIX;
  return gzip;
}

async function packOne(
  spec: PackageSpec,
  specs: readonly PackageSpec[],
  options: PackReleaseOptions,
  baseUrl: string,
  license: Buffer,
): Promise<PackedPackage> {
  const packageDir = join(SCRIPT_DIR, "..", spec.directory);
  const source: unknown = JSON.parse(await readFile(join(packageDir, "package.json"), "utf8"));
  if (!isRecord(source) || source["name"] !== spec.name) {
    throw new Error(`${spec.directory}/package.json must be named ${spec.name}`);
  }
  const rewritten = rewriteDependencies(source, specs, baseUrl, spec.name);
  const manifest: JsonObject = {};
  for (const [key, value] of Object.entries(rewritten)) {
    manifest[key] = value;
    if (key === "name") {
      manifest["version"] = options.version;
    }
  }
  manifest["license"] = LICENSE_IDENTIFIER;

  const entries = new Map<string, Buffer>();
  for (const directory of PUBLISHED_DIRECTORIES) {
    if (await exists(join(packageDir, directory))) {
      const files: string[] = [];
      await collectFiles(packageDir, directory, files);
      for (const file of files) {
        entries.set(file, await readFile(join(packageDir, file)));
      }
    }
  }
  if (await exists(join(packageDir, README_FILE))) {
    entries.set(README_FILE, await readFile(join(packageDir, README_FILE)));
  }
  entries.set(LICENSE_FILE, license);
  assertExportsPacked(manifest, [...entries.keys()], spec.name);
  entries.set("package.json", Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`));

  const tarball = buildTarball(entries, options.sourceDateEpoch);
  const sha256 = createHash("sha256").update(tarball).digest("hex");
  const tarballPath = join(options.outDir, spec.asset);
  await writeFile(tarballPath, tarball);
  await writeFile(`${tarballPath}.sha256`, `${sha256}  ${spec.asset}\n`);
  return {
    name: spec.name,
    tarball: tarballPath,
    checksum: `${tarballPath}.sha256`,
    sha256,
    files: [...entries.keys()].sort(byteOrder),
  };
}

export async function packRelease(options: PackReleaseOptions): Promise<readonly PackedPackage[]> {
  const baseUrl = validateOptions(options);
  const specs = PACKAGE_DIRECTORIES.map(
    (directory): PackageSpec => ({
      directory,
      name: `${SCOPE_PREFIX}${directory}`,
      asset: assetName(directory, options.version),
    }),
  );
  const license = await readFile(join(REPOSITORY_ROOT, LICENSE_FILE));
  await mkdir(options.outDir, { recursive: true });
  const packed: PackedPackage[] = [];
  for (const spec of specs) {
    packed.push(await packOne(spec, specs, options, baseUrl, license));
  }
  return packed;
}

function flagValue(args: readonly string[], flag: string): string {
  const index = args.indexOf(flag);
  const value = index === -1 ? undefined : args[index + 1];
  if (value === undefined || value.startsWith("--")) {
    throw new Error(`missing required ${flag} <value>`);
  }
  return value;
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const epoch = process.env["SOURCE_DATE_EPOCH"];
  if (epoch === undefined || !/^\d+$/u.test(epoch)) {
    throw new Error("SOURCE_DATE_EPOCH must be set to a Unix timestamp");
  }
  const packed = await packRelease({
    version: flagValue(args, "--version"),
    baseUrl: flagValue(args, "--base-url"),
    outDir: flagValue(args, "--out"),
    sourceDateEpoch: Number(epoch),
  });
  for (const item of packed) {
    process.stdout.write(`${basename(item.tarball)} ${item.sha256}\n`);
  }
}

if (import.meta.main) {
  main().catch((error: unknown) => {
    process.stderr.write(`pack-release: ${error instanceof Error ? error.message : String(error)}\n`);
    process.exit(1);
  });
}
