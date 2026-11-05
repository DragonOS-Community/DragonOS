import { viteBundler } from '@vuepress/bundler-vite'
import { defaultTheme } from '@vuepress/theme-default'
import { defineUserConfig } from 'vuepress'

export default defineUserConfig({
    title: 'DADK文档',
    description: 'DragonOS Application Development Kit',
    bundler: viteBundler(),
    base: process.env.NODE_ENV === 'production' ? '/p/dadk/' : '/',
    theme: defaultTheme(
        {
            repo: 'DragonOS-Community/DADK',
            repoLabel: 'GitHub',
            editLinks: true,
            // 默认为 "Edit this page"
            editLinkText: '帮助我们改善此页面！',
            smoothScroll: true,
            docsBranch: 'main',
            nextLinks: true,
            logo: 'https://static.dragonos.org.cn/casdoor/dragonos_en_pic.png',
            docsDir: 'docs',
            navbar: [
                {
                    text: '首页',
                    link: '/',
                },
                {
                    text: '用户指南',
                    link: '/user-manual/',
                },
                {
                    text: '开发者指南',
                    link: '/dev-guide/',
                },
            ],
            sidebar: {
                '/': 'heading',
                '/dev-guide/': [
                    {
                        text: '开发者指南',
                        children: [
                            '/dev-guide/README.md',
                            '/dev-guide/how-to-write-docs.md',
                        ],
                    },
                ],
                '/user-manual/': [
                    {
                        text: '用户指南',
                        children: [
                            '/user-manual/quickstart.md',
                            '/user-manual/rootfs-base.md',
                            '/user-manual/profiling.md',
                            '/user-manual/user-prog-build.md',
                            '/user-manual/app-blocklist.md',
                            '/user-manual/envs.md',
                        ]
                    }
                ]
            }
            
        }
    ),
})
