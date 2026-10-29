import { existsSync, readdirSync, statSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vitepress'
import { withMermaid } from 'vitepress-plugin-mermaid'
import gdbGrammar from './grammars/gdb.tmLanguage.json'
import { omitUnusedMermaidDiagrams } from './plugins/omit-unused-mermaid'
import { serveLegacyArchives } from './plugins/serve-legacy'
import { enSidebar } from './sidebars/en'
import { zhSidebar } from './sidebars/zh'

const repo = 'https://github.com/DragonOS-Community/DragonOS'
const repoRoot = fileURLToPath(new URL('../..', import.meta.url))
const archiveHtmlDir = fileURLToPath(new URL('./archives/html', import.meta.url))

function localArchiveTags(dir: string): string[] {
  if (!existsSync(dir)) return []
  return readdirSync(dir).filter(
    (name: string) => /^V[\d.]+$/.test(name) && statSync(join(dir, name)).isDirectory(),
  )
}

export default withMermaid(
  defineConfig({
    title: 'DragonOS',
    description: 'Documentation for the DragonOS operating system',
    lastUpdated: true,
    cleanUrls: false,
    ignoreDeadLinks: true,
    srcExclude: [
      'agents/**',
      'analysis/**',
      'bugfix/**',
      'issues/**',
      'review/**',
      'design/**',
      'locales/**',
      '_templates/**',
      '_build/**',
      '_static/**',
    ],
    mermaid: {
      securityLevel: 'loose',
    },
    markdown: {
      html: false,
      languages: [gdbGrammar],
    },
    head: [
      ['link', { rel: 'icon', href: '/favicon.svg', type: 'image/svg+xml' }],
      ['link', { rel: 'icon', href: '/favicon.ico', sizes: '32x32' }],
    ],
    themeConfig: {
      logo: '/dragonos-logo.svg',
      siteTitle: false,
      socialLinks: [{ icon: 'github', link: repo }],
      search: {
        provider: 'local',
        options: {
          locales: {
            zh: {
              translations: {
                button: { buttonText: '搜索文档', buttonAriaLabel: '搜索文档' },
                modal: {
                  noResultsText: '没有找到结果',
                  resetButtonTitle: '清除查询',
                  footer: { selectText: '选择', navigateText: '切换', closeText: '关闭' },
                },
              },
            },
          },
        },
      },
    },
    locales: {
      root: {
        label: 'English',
        lang: 'en',
        themeConfig: {
          nav: [
            { text: 'Home', link: '/' },
            { text: 'Kernel', link: '/kernel/configuration/' },
          ],
          sidebar: enSidebar,
          outline: { label: 'On this page' },
          editLink: {
            pattern: `${repo}/edit/master/docs/:path`,
            text: 'Edit this page on GitHub',
          },
          lastUpdated: { text: 'Last updated' },
          docFooter: { prev: 'Previous', next: 'Next' },
        },
      },
      zh: {
        label: '简体中文',
        lang: 'zh-CN',
        themeConfig: {
          nav: [
            { text: '首页', link: '/zh/' },
            { text: '内核', link: '/zh/kernel/configuration/' },
          ],
          sidebar: zhSidebar,
          outline: { label: '本页目录' },
          editLink: {
            pattern: `${repo}/edit/master/docs/:path`,
            text: '在 GitHub 上编辑此页',
          },
          lastUpdated: { text: '最后更新' },
          docFooter: { prev: '上一篇', next: '下一篇' },
        },
      },
    },
    vite: {
      plugins: [omitUnusedMermaidDiagrams(), serveLegacyArchives(archiveHtmlDir)],
      define: {
        __DOC_LOCAL_ARCHIVE_TAGS__: JSON.stringify(localArchiveTags(archiveHtmlDir)),
      },
      server: {
        fs: {
          allow: [repoRoot],
        },
      },
      build: {
        // MiniSearch locale indexes are lazy ~1.2MB chunks; 500kB default is for entry bundles.
        chunkSizeWarningLimit: 1300,
      },
      optimizeDeps: {
        include: [
          'debug',
          'mermaid',
        ],
      },
    },
  }),
)
