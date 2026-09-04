<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useData, useRoute } from 'vitepress'
import legacyTags from '../legacy-tags.json'

declare const __DOC_LOCAL_ARCHIVE_TAGS__: string[]

const PRODUCTION_DOCS = 'https://docs.dragonos.org.cn'

const LEGACY_TAGS = legacyTags as readonly string[]

type VersionId = 'latest' | (typeof LEGACY_TAGS)[number]

const { localeIndex } = useData()
const route = useRoute()
const open = ref(false)

const versions: { id: VersionId; text: string }[] = [
  { id: 'latest', text: 'latest' },
  ...LEGACY_TAGS.map((id) => ({ id, text: id })),
]

function stripPrefix(path: string, prefix: string): string {
  return path.startsWith(prefix) ? path.slice(prefix.length) : path
}

function parseDocLocation(pathname: string): { version: VersionId; locale: 'zh' | 'en'; page: string } {
  let rest = pathname || '/'
  let version: VersionId = 'latest'
  const tagged = rest.match(/^\/(V[\d.]+)(?:\/(.*))?$/)
  if (tagged) {
    version = tagged[1] as VersionId
    rest = tagged[2] ? `/${tagged[2]}` : '/'
  } else if (rest === '/master' || rest.startsWith('/master/')) {
    rest = rest.slice('/master'.length) || '/'
  }

  let locale: 'zh' | 'en' = 'en'
  if (rest === '/zh' || rest.startsWith('/zh/')) {
    locale = 'zh'
    rest = stripPrefix(rest, '/zh') || '/'
  } else if (rest === '/locales/en' || rest.startsWith('/locales/en/')) {
    locale = 'en'
    rest = stripPrefix(rest, '/locales/en') || '/'
  } else if (version !== 'latest') {
    locale = 'zh'
  }

  let page = rest.replace(/^\/+/, '')
  if (page === 'index.html') page = ''
  if (page && !page.endsWith('/') && !/\.[A-Za-z0-9]+$/.test(page)) {
    page += '.html'
  }
  return { version, locale, page }
}

function hrefFor(target: VersionId, locale: 'zh' | 'en', page: string): string {
  const suffix = page
  if (target === 'latest') {
    if (locale === 'zh') return suffix ? `/zh/${suffix}` : '/zh/'
    return suffix ? `/${suffix}` : '/'
  }
  if (locale === 'zh') return suffix ? `/${target}/${suffix}` : `/${target}/`
  return suffix ? `/${target}/locales/en/${suffix}` : `/${target}/locales/en/`
}

const current = computed(() => parseDocLocation(route.path))
const currentId = computed(() => current.value.version)
const ariaLabel = computed(() => (localeIndex.value === 'zh' ? '版本' : 'Version'))

function itemHref(id: VersionId): string {
  const path = hrefFor(id, current.value.locale, current.value.page)
  const localTags = typeof __DOC_LOCAL_ARCHIVE_TAGS__ === 'undefined' ? [] : __DOC_LOCAL_ARCHIVE_TAGS__
  if (id !== 'latest' && import.meta.env.DEV && !localTags.includes(id)) {
    return `${PRODUCTION_DOCS}${path}`
  }
  return path
}

function close() {
  open.value = false
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Escape') close()
}

onMounted(() => document.addEventListener('keydown', onKeydown))
onUnmounted(() => document.removeEventListener('keydown', onKeydown))
</script>

<template>
  <div
    class="version-switcher"
    @mouseenter="open = true"
    @mouseleave="open = false"
  >
    <button
      type="button"
      class="version-switcher__button"
      :aria-label="ariaLabel"
      :aria-expanded="open"
      aria-haspopup="listbox"
      @click="open = !open"
    >
      <span>{{ currentId }}</span>
      <span class="version-switcher__chevron" aria-hidden="true" />
    </button>
    <ul v-show="open" class="version-switcher__menu" role="listbox">
      <li v-for="item in versions" :key="item.id" role="none">
        <a
          class="version-switcher__item"
          :class="{ 'is-active': item.id === currentId }"
          :href="itemHref(item.id)"
          target="_self"
          role="option"
          :aria-selected="item.id === currentId"
          @click="close"
        >
          {{ item.text }}
        </a>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.version-switcher {
  position: relative;
  display: flex;
  align-items: center;
  height: var(--vp-nav-height);
  margin-right: 8px;
}

.version-switcher__button {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 0 8px;
  height: 36px;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: var(--vp-c-text-1);
  font-size: 13px;
  font-weight: 500;
  line-height: 1;
  cursor: pointer;
}

.version-switcher__button:hover {
  background: var(--vp-c-bg-soft);
}

.version-switcher__chevron {
  width: 0;
  height: 0;
  border-left: 4px solid transparent;
  border-right: 4px solid transparent;
  border-top: 5px solid var(--vp-c-text-2);
}

.version-switcher__menu {
  position: absolute;
  top: calc(var(--vp-nav-height) - 8px);
  right: 0;
  z-index: 30;
  min-width: 132px;
  max-height: min(70vh, 420px);
  margin: 0;
  padding: 6px;
  overflow: auto;
  list-style: none;
  border: 1px solid var(--vp-c-divider);
  border-radius: 12px;
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-3);
}

.version-switcher__item {
  display: block;
  padding: 6px 10px;
  border-radius: 6px;
  color: var(--vp-c-text-1);
  font-size: 13px;
  line-height: 1.4;
  text-decoration: none;
  white-space: nowrap;
}

.version-switcher__item:hover {
  background: var(--vp-c-bg-soft);
}

.version-switcher__item.is-active {
  color: var(--vp-c-brand-1);
  font-weight: 600;
}
</style>
