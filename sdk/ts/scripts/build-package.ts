// Compiles one SDK workspace package into the files a release tarball ships.
//
// The workspace sources use extensionless relative imports and `exports` that
// point at `src/*.ts`, which only a bundler-style resolver can load. The
// published artifact is plain ESM that Node resolves natively: one `.js` and
// one `.d.ts` per source file, with every relative specifier rewritten to an
// explicit `.js` path (a directory import becomes `<dir>/index.js`). Declarations
// therefore resolve under both `moduleResolution: "bundler"` and `"nodenext"`.
//
// One TypeScript program emits both outputs, in memory, so nothing is written
// into the workspace and the result depends only on the sources and the
// compiler version.

import { existsSync } from "node:fs";
import { readdir } from "node:fs/promises";
import { dirname, join, posix, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import * as ts from "typescript";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const REPOSITORY_ROOT = join(SCRIPT_DIR, "..", "..", "..");
const BASE_TSCONFIG = join(REPOSITORY_ROOT, "tsconfig.base.json");
const TYPE_ROOTS = [join(REPOSITORY_ROOT, "node_modules", "@types")];

export const JS_DIRECTORY = "dist";
export const TYPES_DIRECTORY = "types";
const SOURCE_DIRECTORY = "src";
const SOURCE_EXTENSION = ".ts";
const SOURCE_TARGET = /^\.\/src\/(.+)\.ts$/u;

export type BuiltFiles = ReadonlyMap<string, Buffer>;

async function listSources(directory: string, into: string[]): Promise<void> {
  const entries = await readdir(directory, { withFileTypes: true });
  for (const entry of entries) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) {
      await listSources(path, into);
    } else if (entry.isFile() && entry.name.endsWith(SOURCE_EXTENSION) && !entry.name.endsWith(".d.ts")) {
      into.push(path);
    } else if (!entry.isFile()) {
      throw new Error(`refusing to build a non-regular file: ${path}`);
    }
  }
}

function compilerOptions(): ts.CompilerOptions {
  const raw = ts.readConfigFile(BASE_TSCONFIG, (path) => ts.sys.readFile(path));
  if (raw.error !== undefined) {
    throw new Error(ts.flattenDiagnosticMessageText(raw.error.messageText, "\n"));
  }
  const parsed = ts.convertCompilerOptionsFromJson(
    (raw.config as { compilerOptions: object }).compilerOptions,
    REPOSITORY_ROOT,
  );
  if (parsed.errors.length > 0) {
    throw new Error(parsed.errors.map((error) => ts.flattenDiagnosticMessageText(error.messageText, "\n")).join("\n"));
  }
  return {
    ...parsed.options,
    noEmit: false,
    declaration: true,
    composite: false,
    sourceMap: false,
    declarationMap: false,
    removeComments: false,
    newLine: ts.NewLineKind.LineFeed,
    typeRoots: TYPE_ROOTS,
  };
}

// `./x` becomes `./x.js` when `x.ts` exists and `./x/index.js` when `x/index.ts`
// does; any other relative specifier means the source layout is not one this
// build understands.
function explicitSpecifier(specifier: string, fromFile: string): string {
  if (!specifier.startsWith("./") && !specifier.startsWith("../")) {
    return specifier;
  }
  const absolute = resolve(dirname(fromFile), specifier);
  if (existsSync(`${absolute}${SOURCE_EXTENSION}`)) {
    return `${specifier}.js`;
  }
  if (existsSync(join(absolute, `index${SOURCE_EXTENSION}`))) {
    return `${specifier.replace(/\/+$/u, "")}/index.js`;
  }
  throw new Error(`cannot resolve relative specifier "${specifier}" imported by ${fromFile}`);
}

function specifierRewriter(): ts.TransformerFactory<ts.SourceFile | ts.Bundle> {
  return (context) => {
    const { factory } = context;
    return (root) => {
      const rewriteFile = (file: ts.SourceFile): ts.SourceFile => {
        const original = ts.getOriginalNode(file);
        const fromFile = ts.isSourceFile(original) ? original.fileName : file.fileName;
        const rewrite = (literal: ts.StringLiteral): ts.StringLiteral =>
          factory.createStringLiteral(explicitSpecifier(literal.text, fromFile));
        const visit = (node: ts.Node): ts.Node => {
          if (ts.isImportDeclaration(node) && ts.isStringLiteral(node.moduleSpecifier)) {
            return factory.updateImportDeclaration(
              node,
              node.modifiers,
              node.importClause,
              rewrite(node.moduleSpecifier),
              node.attributes,
            );
          }
          if (ts.isExportDeclaration(node) && node.moduleSpecifier !== undefined && ts.isStringLiteral(node.moduleSpecifier)) {
            return factory.updateExportDeclaration(
              node,
              node.modifiers,
              node.isTypeOnly,
              node.exportClause,
              rewrite(node.moduleSpecifier),
              node.attributes,
            );
          }
          if (ts.isImportTypeNode(node) && ts.isLiteralTypeNode(node.argument) && ts.isStringLiteral(node.argument.literal)) {
            return factory.updateImportTypeNode(
              node,
              factory.updateLiteralTypeNode(node.argument, rewrite(node.argument.literal)),
              node.attributes,
              node.qualifier,
              node.typeArguments,
              node.isTypeOf,
            );
          }
          if (
            ts.isCallExpression(node) &&
            node.expression.kind === ts.SyntaxKind.ImportKeyword &&
            node.arguments[0] !== undefined &&
            ts.isStringLiteral(node.arguments[0])
          ) {
            return factory.updateCallExpression(node, node.expression, node.typeArguments, [
              rewrite(node.arguments[0]),
              ...node.arguments.slice(1),
            ]);
          }
          return ts.visitEachChild(node, visit, context);
        };
        return ts.visitEachChild(file, visit, context);
      };
      return ts.isBundle(root) ? factory.updateBundle(root, root.sourceFiles.map(rewriteFile)) : rewriteFile(root);
    };
  };
}

function formatDiagnostics(diagnostics: readonly ts.Diagnostic[]): string {
  return ts.formatDiagnostics(diagnostics, {
    getCanonicalFileName: (name) => name,
    getCurrentDirectory: () => REPOSITORY_ROOT,
    getNewLine: () => "\n",
  });
}

/**
 * Type-checks and emits `<packageDir>/src`. Returns `dist/**\/*.js` and
 * `types/**\/*.d.ts` keyed by package-relative POSIX path.
 */
export async function buildPackage(packageDir: string): Promise<BuiltFiles> {
  const sourceDir = join(packageDir, SOURCE_DIRECTORY);
  const sources: string[] = [];
  await listSources(sourceDir, sources);
  sources.sort();
  const program = ts.createProgram({ rootNames: sources, options: compilerOptions() });
  const diagnostics = ts.getPreEmitDiagnostics(program).filter((item) => item.category === ts.DiagnosticCategory.Error);
  if (diagnostics.length > 0) {
    throw new Error(`type errors in ${packageDir}:\n${formatDiagnostics(diagnostics)}`);
  }

  const built = new Map<string, Buffer>();
  const rewriter = specifierRewriter();
  for (const source of sources) {
    const file = program.getSourceFile(source);
    if (file === undefined) {
      throw new Error(`source file missing from the program: ${source}`);
    }
    const stem = relative(sourceDir, source).split(sep).join(posix.sep).slice(0, -SOURCE_EXTENSION.length);
    const result = program.emit(
      file,
      (fileName, text) => {
        if (fileName.endsWith(".d.ts")) {
          built.set(posix.join(TYPES_DIRECTORY, `${stem}.d.ts`), Buffer.from(text));
        } else if (fileName.endsWith(".js")) {
          built.set(posix.join(JS_DIRECTORY, `${stem}.js`), Buffer.from(text));
        } else {
          throw new Error(`unexpected emit output ${fileName}`);
        }
      },
      undefined,
      false,
      { before: [rewriter as ts.TransformerFactory<ts.SourceFile>], afterDeclarations: [rewriter] },
    );
    if (result.emitSkipped) {
      throw new Error(`emit skipped for ${source}:\n${formatDiagnostics(result.diagnostics)}`);
    }
  }
  return built;
}

/** Maps a workspace export target `./src/X.ts` to its built `[types, js]` pair. */
export function builtTargets(target: string): { readonly types: string; readonly js: string } {
  const match = SOURCE_TARGET.exec(target);
  if (match === null) {
    throw new Error(`export target ${target} is not a ./${SOURCE_DIRECTORY}/*${SOURCE_EXTENSION} source`);
  }
  return {
    types: `./${TYPES_DIRECTORY}/${match[1]}.d.ts`,
    js: `./${JS_DIRECTORY}/${match[1]}.js`,
  };
}
