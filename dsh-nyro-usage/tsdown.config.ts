import { readFile } from 'node:fs/promises'
import { dirname, resolve as resolvePath } from 'node:path'
import { defineConfig } from 'tsdown'
import type { Plugin } from 'rolldown'

/**
 * Inline CSS modules: the dsh client loader only ever fetches `client.js`,
 * so a `.module.css` import must become (a) a class-name map export and
 * (b) a one-shot `<style>` injection riding the JS — the same output shape
 * the dsh-web-ui family's client bundles ship.
 *
 * Implemented as resolveId→virtual-js + load: tsdown's built-in css-guard
 * throws on any `.css`-suffixed module id when `@tsdown/css` is absent, so
 * the css file is swapped for a `\0`-virtual `.js` module before any
 * transform sees it.
 *
 * Class names are prefixed (`nyu-`) so nothing leaks into the shell; the
 * stylesheet is tagged per style-tag and injected at most once.
 */
const VIRTUAL_PREFIX = '\0dsh-nyro-usage-css:'

function inlineCssModules(prefix: string, styleTag: string): Plugin {
  const escapeRegExp = (value: string): string => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const generate = (code: string): string => {
    const names = new Set<string>()
    for (const match of code.matchAll(/(^|[\s,{}>+~])\.([A-Za-z_][A-Za-z0-9_-]*)/g)) {
      names.add(match[2])
    }
    let css = code
    const map: Record<string, string> = {}
    for (const name of names) {
      const mapped = `${prefix}${name}`
      map[name] = mapped
      css = css.replace(new RegExp(`\\.${escapeRegExp(name)}(?![\\w-])`, 'g'), `.${mapped}`)
    }
    return [
      `const cssText = ${JSON.stringify(css)};`,
      `if (typeof document !== 'undefined') {`,
      `  const tag = ${JSON.stringify(styleTag)};`,
      `  if (document.querySelector('style[' + tag + ']') === null) {`,
      `    const el = document.createElement('style');`,
      `    el.setAttribute(tag, '');`,
      `    el.textContent = cssText;`,
      `    document.head.appendChild(el);`,
      `  }`,
      `}`,
      `export default ${JSON.stringify(map)};`,
    ].join('\n')
  }
  return {
    name: 'dsh-nyro-usage:inline-css-modules',
    async resolveId(source, importer) {
      if (!source.endsWith('.module.css')) return undefined
      const base = importer === undefined ? process.cwd() : dirname(importer)
      return { id: `${VIRTUAL_PREFIX}${resolvePath(base, source)}.js`, importer: undefined }
    },
    async load(id) {
      if (!id.startsWith(VIRTUAL_PREFIX)) return undefined
      const file = id.slice(VIRTUAL_PREFIX.length, -'.js'.length)
      return generate(await readFile(file, 'utf8'))
    },
  }
}

/** Runtime-provided imports must never be bundled. */
const EXTERNAL = [/^@deepseek-ai\//, /^react(\/.*)?$/, /^react-dom(\/.*)?$/, /^schemastery$/]

/**
 * Two entries, matching the package exports:
 * - `index`  → lib/index.js   the host half (nyro Admin API proxy routes)
 * - `client` → lib/client.js  the browser half (sidebar entry + usage panel)
 *
 * Everything @deepseek-ai/* and react stays external: the host loader and
 * the client runtime provide them at run time.
 */
export default defineConfig([
  {
    entry: { index: 'src/index.ts' },
    outDir: 'lib',
    format: 'esm',
    platform: 'neutral',
    dts: false,
    external: EXTERNAL,
    plugins: [],
    outExtensions: () => ({ js: '.js' }),
  },
  {
    entry: { client: 'src/client/index.ts' },
    outDir: 'lib',
    // The dsh client module loader consumes the family wrapper format:
    // `window.__ModuleLoader__.load({ id, factory: (require) => { …cjs… } })`.
    // Externals resolve through the factory's require; named ESM exports
    // land as `exports.apply` / `exports.inject`.
    format: 'cjs',
    platform: 'browser',
    dts: false,
    external: EXTERNAL,
    plugins: [inlineCssModules('nyu-', 'data-dsh-nyro-usage-css')],
    outExtensions: () => ({ js: '.js' }),
    banner: {
      js: "window.__ModuleLoader__.load({\n\tid: 'dsh-nyro-usage',\n\tfactory: (require) => {\n\t\tvar module = { exports: {} };\n\t\tvar exports = module.exports;",
    },
    footer: {
      js: "\t\treturn module.exports;\n\t}\n});",
    },
  },
])
