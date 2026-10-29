import type { Plugin } from 'vite'

/** Docs only use flowchart / graph / sequence / state / class. Drop unused Mermaid diagram bundles. */
const UNUSED = /(?:flowchart-elk-definition|mindmap-definition)/

export function omitUnusedMermaidDiagrams(): Plugin {
  return {
    name: 'omit-unused-mermaid-diagrams',
    enforce: 'pre',
    load(id) {
      if (!UNUSED.test(id)) return
      return 'export const diagram = { id: "omitted" };\nexport default {};\n'
    },
  }
}
