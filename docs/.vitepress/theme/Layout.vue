<script setup lang="ts">
import DefaultTheme from 'vitepress/theme'
import VersionSwitcher from './VersionSwitcher.vue'
</script>

<template>
  <DefaultTheme.Layout>
    <template #nav-bar-content-after>
      <VersionSwitcher />
    </template>
    <template #layout-bottom>
      <footer class="dragonos-footer">
        <a href="https://dragonos.org">https://dragonos.org</a>
        <span>|</span>
        <a href="https://dragonos.org">Sponsor DragonOS</a>
      </footer>
    </template>
  </DefaultTheme.Layout>
</template>
