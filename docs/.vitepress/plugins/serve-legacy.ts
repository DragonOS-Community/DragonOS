import { createReadStream, existsSync, statSync } from 'node:fs'
import { extname, join, resolve } from 'node:path'
import type { IncomingMessage, ServerResponse } from 'node:http'
import type { Connect, Plugin, PreviewServer, ViteDevServer } from 'vite'

const PRODUCTION_DOCS = 'https://docs.dragonos.org.cn'

const MIME: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.ttf': 'font/ttf',
  '.eot': 'application/vnd.ms-fontobject',
  '.map': 'application/json',
  '.txt': 'text/plain; charset=utf-8',
  '.xml': 'application/xml',
}

function safeJoin(root: string, rel: string): string | null {
  const abs = resolve(root, rel)
  const prefix = root.endsWith('/') ? root : `${root}/`
  if (abs !== root && !abs.startsWith(prefix)) return null
  return abs
}

function tryFile(abs: string | null): string | null {
  if (!abs || !existsSync(abs)) return null
  const st = statSync(abs)
  if (st.isFile()) return abs
  if (st.isDirectory()) {
    const index = join(abs, 'index.html')
    if (existsSync(index) && statSync(index).isFile()) return index
  }
  return null
}

function resolveArchiveFile(archiveRoot: string, urlPath: string): string | null {
  let rel = urlPath.replace(/^\/+/, '')
  if (!rel || rel.endsWith('/')) rel = `${rel}index.html`
  const direct = tryFile(safeJoin(archiveRoot, rel))
  if (direct) return direct
  if (!rel.endsWith('.html')) {
    const withHtml = tryFile(safeJoin(archiveRoot, `${rel}.html`))
    if (withHtml) return withHtml
  }
  return null
}

function resolveSharedFont(sharedFontsDir: string, urlPath: string): string | null {
  const rel = urlPath.replace(/^\/+/, '')
  const match = rel.match(/(?:^|\/)_static\/css\/fonts\/([^/]+)$/)
  if (!match) return null
  return tryFile(safeJoin(sharedFontsDir, match[1]))
}

function isDocumentPath(urlPath: string): boolean {
  return urlPath.endsWith('.html') || urlPath.endsWith('/') || !extname(urlPath)
}

async function proxyProductionAsset(raw: string, method: string | undefined, res: ServerResponse) {
  try {
    const upstream = await fetch(`${PRODUCTION_DOCS}${raw}`, {
      method: method === 'HEAD' ? 'HEAD' : 'GET',
      redirect: 'follow',
    })
    if (!upstream.ok) {
      res.statusCode = upstream.status
      res.end('Not Found')
      return
    }
    res.statusCode = 200
    const contentType = upstream.headers.get('content-type')
    if (contentType) res.setHeader('Content-Type', contentType)
    if (method === 'HEAD') {
      res.end()
      return
    }
    res.end(Buffer.from(await upstream.arrayBuffer()))
  } catch {
    res.statusCode = 502
    res.end('Bad Gateway')
  }
}

function attachLegacyMiddleware(archiveHtmlDir: string, sharedFontsDir: string) {
  return (req: IncomingMessage, res: ServerResponse, next: Connect.NextFunction) => {
    const raw = req.url?.split('?')[0] ?? ''
    const match = raw.match(/^\/(V[\d.]+)(\/.*)?$/)
    if (!match) {
      next()
      return
    }

    const tag = match[1]
    const rest = match[2] || '/'
    const archiveRoot = resolve(archiveHtmlDir, tag)
    if (!existsSync(archiveRoot) || !statSync(archiveRoot).isDirectory()) {
      res.statusCode = 302
      res.setHeader('Location', `${PRODUCTION_DOCS}${raw}`)
      res.end()
      return
    }

    const file = resolveArchiveFile(archiveRoot, rest) ?? resolveSharedFont(sharedFontsDir, rest)
    if (!file) {
      // 不能把首页 HTML 挂在深层 URL 上：Sphinx 用相对 _static，样式会全部 404。
      if (isDocumentPath(rest)) {
        const home = rest.includes('/locales/en') ? `/${tag}/locales/en/` : `/${tag}/`
        res.statusCode = 302
        res.setHeader('Location', home)
        res.end()
        return
      }
      // wget 镜像通常拿不到 CSS url() 里的字体；同源回源现网，避免跨域 @font-face 失败。
      void proxyProductionAsset(raw, req.method, res)
      return
    }

    res.statusCode = 200
    res.setHeader('Content-Type', MIME[extname(file).toLowerCase()] ?? 'application/octet-stream')
    if (req.method === 'HEAD') {
      res.end()
      return
    }
    createReadStream(file).pipe(res)
  }
}

export function serveLegacyArchives(archiveHtmlDir: string): Plugin {
  const sharedFontsDir = resolve(archiveHtmlDir, '../shared/rtd-theme-fonts')
  const hook = (server: ViteDevServer | PreviewServer) => {
    server.middlewares.use(attachLegacyMiddleware(archiveHtmlDir, sharedFontsDir))
  }

  return {
    name: 'dragonos-serve-legacy-archives',
    configureServer: hook,
    configurePreviewServer: hook,
  }
}
