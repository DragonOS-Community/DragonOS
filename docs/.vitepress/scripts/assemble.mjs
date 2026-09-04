#!/usr/bin/env node
/**
 * Overlay frozen version HTML from the repo onto the VitePress dist, copy
 * latest to /master/, and emit refresh stubs for legacy /locales/en/ URLs.
 *
 * Historical pages come only from docs/.vitepress/archives/html/<tag>/.
 */
import { createWriteStream, existsSync, mkdirSync, readdirSync, readFileSync, statSync, cpSync, rmSync } from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const docsDir = resolve(here, '../..')
const distDir = resolve(docsDir, '.vitepress/dist')
const archiveHtmlDir = resolve(docsDir, '.vitepress/archives/html')
const sharedFontsDir = resolve(docsDir, '.vitepress/archives/shared/rtd-theme-fonts')
const LEGACY_TAGS = JSON.parse(
  readFileSync(resolve(here, '../legacy-tags.json'), 'utf8'),
)

function fail(msg) {
  console.error(msg)
  process.exit(1)
}

if (!existsSync(distDir)) {
  fail(`VitePress dist not found: ${distDir}. Run npm run docs:build first.`)
}

if (existsSync(join(distDir, 'p/dadk'))) {
  fail('Refusing to assemble: dist contains p/dadk which belongs to another site.')
}

function collectHtmlFiles(root, acc = []) {
  if (!existsSync(root)) return acc
  for (const name of readdirSync(root)) {
    const p = join(root, name)
    const st = statSync(p)
    if (st.isDirectory()) collectHtmlFiles(p, acc)
    else if (name.endsWith('.html')) acc.push(p)
  }
  return acc
}

function writeRedirect(fromAbs, toUrl) {
  mkdirSync(dirname(fromAbs), { recursive: true })
  const html = `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta http-equiv="refresh" content="0; url=${toUrl}">
  <link rel="canonical" href="${toUrl}">
  <title>Redirecting</title>
</head>
<body>
  <p>Redirecting to <a href="${toUrl}">${toUrl}</a></p>
  <script>location.replace(${JSON.stringify(toUrl)})</script>
</body>
</html>
`
  createWriteStream(fromAbs).end(html)
}

function emitLocaleEnRedirects(baseDir, urlPrefix = '') {
  const htmlFiles = collectHtmlFiles(baseDir).filter((p) => {
    const rel = relative(baseDir, p).replace(/\\/g, '/')
    return !rel.startsWith('zh/') && !rel.startsWith('V') && !rel.startsWith('master/') && !rel.startsWith('locales/')
  })
  for (const file of htmlFiles) {
    const rel = relative(baseDir, file).replace(/\\/g, '/')
    const destUrl = `${urlPrefix}/${rel}`.replace(/\/{2,}/g, '/')
    writeRedirect(join(baseDir, 'locales/en', rel), destUrl)
  }
  writeRedirect(join(baseDir, 'locales/en/index.html'), `${urlPrefix}/index.html`.replace(/\/{2,}/g, '/') || '/index.html')
}

function emitZhCnLatestRedirects(baseDir) {
  const zhRoot = join(baseDir, 'zh')
  for (const file of collectHtmlFiles(zhRoot)) {
    const rel = relative(zhRoot, file).replace(/\\/g, '/')
    writeRedirect(join(baseDir, 'zh_CN/latest', rel), `/zh/${rel}`)
  }
}

function overlayLegacy() {
  let found = 0
  for (const tag of LEGACY_TAGS) {
    const src = join(archiveHtmlDir, tag)
    const index = join(src, 'index.html')
    if (!existsSync(index) || !statSync(index).isFile()) {
      fail(`Frozen archive missing: ${src}/index.html. Commit archives/html/${tag}/ to the repo.`)
    }
    const dest = join(distDir, tag)
    if (existsSync(dest)) rmSync(dest, { recursive: true, force: true })
    cpSync(src, dest, { recursive: true })
    if (existsSync(join(dest, '_static/css/theme.css'))) {
      if (!existsSync(sharedFontsDir)) {
        fail(`Shared RTD fonts missing: ${sharedFontsDir}`)
      }
      cpSync(sharedFontsDir, join(dest, '_static/css/fonts'), { recursive: true, dereference: true })
    }
    found += 1
    console.log(`Overlay ${tag}`)
  }
  console.log(`Overlayed ${found}/${LEGACY_TAGS.length} frozen versions`)
}

function copyLatestToMaster() {
  const master = join(distDir, 'master')
  if (existsSync(master)) rmSync(master, { recursive: true, force: true })
  mkdirSync(master, { recursive: true })
  for (const name of readdirSync(distDir)) {
    if (name === 'master' || LEGACY_TAGS.includes(name) || name === 'p') continue
    const src = join(distDir, name)
    cpSync(src, join(master, name), { recursive: true })
  }
  console.log('Copied latest site to /master/')
}

overlayLegacy()
copyLatestToMaster()
emitLocaleEnRedirects(distDir, '')
emitLocaleEnRedirects(join(distDir, 'master'), '/master')
emitZhCnLatestRedirects(distDir)
emitZhCnLatestRedirects(join(distDir, 'master'))
console.log('Assemble complete.')
